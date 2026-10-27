use std::{ffi::c_void, sync::{atomic::{AtomicBool, Ordering}, Arc}, thread::JoinHandle, time::Duration};

use windows::{core::Interface, Win32::{Media::Audio::{eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVE_FORMAT_PCM}, System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED}}};

use crate::prelude::{AudioCaptureConfig, AudioChannelCount, AudioSampleRate};

pub struct WindowsAudioCaptureStream {
    should_couninit: bool,
    audio_client: IAudioClient,
    stop_flag: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

pub enum WindowsAudioCaptureStreamCreateError {
    Other(String),
    EndpointEnumerationFailed,
    AudioClientActivationFailed,
    AudioClientInitializeFailed,
    AudioCaptureCreationFailed,
    StreamStartFailed,
}

#[derive(Debug)]
pub enum WindowsAudioCaptureStreamError {
    Other(String),
    GetBufferFailed,
}

#[allow(unused)]
pub struct WindowsAudioCaptureStreamPacket<'a> {
    pub(crate) data: &'a [i16],
    pub(crate) channel_count: u32,
    pub(crate) origin_time: Duration,
    pub(crate) duration: Duration,
    pub(crate) sample_index: u64,
}

struct SendCaptureClient(*mut c_void);

unsafe impl Send for SendCaptureClient {}
unsafe impl Sync for SendCaptureClient {}

impl SendCaptureClient {
    fn from_iaudiocaptureclient(client: IAudioCaptureClient) -> Self {
        SendCaptureClient(client.into_raw())
    }

    fn into_iaudiocaptureclient(self) -> IAudioCaptureClient {
        unsafe { IAudioCaptureClient::from_raw(self.0) }
    }
}

impl WindowsAudioCaptureStream {
    pub fn new(config: AudioCaptureConfig, mut callback: Box<dyn for <'a> FnMut(Result<WindowsAudioCaptureStreamPacket<'a>, WindowsAudioCaptureStreamError>) + Send + 'static>) -> Result<Self, WindowsAudioCaptureStreamCreateError> {
        unsafe {
            let should_couninit = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();

            let mm_device_enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| WindowsAudioCaptureStreamCreateError::Other(format!("Failed to create MMDeviceEnumerator: {}", e.to_string())))?;
            let device = mm_device_enumerator.GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|_| WindowsAudioCaptureStreamCreateError::EndpointEnumerationFailed)?;
            
            let audio_client: IAudioClient = device.Activate(CLSCTX_ALL, None)
                .map_err(|_| WindowsAudioCaptureStreamCreateError::AudioClientActivationFailed)?;

            let mut format = WAVEFORMATEX::default();
            format.wFormatTag = WAVE_FORMAT_PCM as u16;
            format.nSamplesPerSec = match config.sample_rate {
                AudioSampleRate::Hz8000  =>  8000,
                AudioSampleRate::Hz16000 => 16000,
                AudioSampleRate::Hz24000 => 24000,
                AudioSampleRate::Hz48000 => 48000,
            };
            format.wBitsPerSample = 16;
            format.nChannels = match config.channel_count {
                AudioChannelCount::Mono   => 1,
                AudioChannelCount::Stereo => 2,
            };
            format.nBlockAlign = format.nChannels * 2;
            format.nAvgBytesPerSec = format.nSamplesPerSec * format.nBlockAlign as u32;
            format.cbSize = 0;

            let callback_format = format.clone();

            let buffer_size = 512;
            let buffer_time = buffer_size as i64 * 10000000i64 / format.nSamplesPerSec as i64;

            let buffer_duration = Duration::from_nanos(buffer_time as u64 * 100);
            let half_buffer_duration = buffer_duration / 2;

            // Shared-mode loopback runs at the engine mix format (usually 32-bit float); without AUTOCONVERTPCM,
            // requesting 16-bit PCM here makes Initialize fail with AUDCLNT_E_UNSUPPORTED_FORMAT.
            let stream_flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            // In shared mode the periodicity argument must be 0
            audio_client.Initialize(AUDCLNT_SHAREMODE_SHARED, stream_flags, buffer_time, 0, &format as *const _, None)
                .map_err(|_| WindowsAudioCaptureStreamCreateError::AudioClientInitializeFailed)?;

            let capture_client : IAudioCaptureClient = audio_client.GetService()
                .map_err(|_| WindowsAudioCaptureStreamCreateError::AudioCaptureCreationFailed)?;

            let capture_client_send = SendCaptureClient::from_iaudiocaptureclient(capture_client);

            let stop_flag = Arc::new(AtomicBool::new(false));
            let worker_stop_flag = stop_flag.clone();
            let channel_count = callback_format.nChannels as usize;
            let sample_rate = callback_format.nSamplesPerSec as u64;

            let worker = std::thread::spawn(move || {
                {
                    let should_couninit = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();

                    // Running count of frames (samples per channel) delivered; timestamps derive from it.
                    let mut frame_count = 0u64;
                    let mut silence: Vec<i16> = Vec::new();

                    let capture_client = capture_client_send.into_iaudiocaptureclient();
                    'outer: while !worker_stop_flag.load(Ordering::Acquire) {
                        std::thread::sleep(half_buffer_duration);

                        // Drain every queued packet; reading one per wake-up dropped audio whenever
                        // more than one packet had accumulated.
                        loop {
                            if worker_stop_flag.load(Ordering::Acquire) {
                                break 'outer;
                            }
                            let packet_frames = match capture_client.GetNextPacketSize() {
                                Ok(count) => count,
                                Err(e) => {
                                    (callback)(Err(WindowsAudioCaptureStreamError::Other(format!("Stream failed - couldn't fetch packet size: {}", e))));
                                    break 'outer;
                                }
                            };
                            if packet_frames == 0 {
                                break;
                            }

                            let mut data_ptr: *mut u8 = std::ptr::null_mut();
                            let mut num_frames = 0u32;
                            let mut flags = 0u32;

                            if capture_client.GetBuffer(&mut data_ptr as *mut _, &mut num_frames as *mut _, &mut flags as *mut _, None, None).is_err() {
                                (callback)(Err(WindowsAudioCaptureStreamError::GetBufferFailed));
                                break 'outer;
                            }
                            if num_frames == 0 {
                                let _ = capture_client.ReleaseBuffer(0);
                                break;
                            }

                            // The slice length is frames * channels (previously hard-coded to * 2, which
                            // over-read the WASAPI buffer for mono streams).
                            let sample_len = num_frames as usize * channel_count;
                            let data: &[i16] = if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data_ptr.is_null() {
                                // Buffer contents are undefined when flagged silent; deliver zeros
                                silence.clear();
                                silence.resize(sample_len, 0);
                                &silence[..]
                            } else {
                                std::slice::from_raw_parts(data_ptr as *const i16, sample_len)
                            };

                            // Timestamps in stream time derived from frame counts. (The old code treated the
                            // device position - a frame index - as 100ns units, and mixed units for duration.)
                            let packet = WindowsAudioCaptureStreamPacket {
                                data,
                                channel_count: channel_count as u32,
                                origin_time: Duration::from_nanos(frame_count * 1_000_000_000 / sample_rate),
                                duration: Duration::from_nanos(num_frames as u64 * 1_000_000_000 / sample_rate),
                                sample_index: frame_count,
                            };
                            (callback)(Ok(packet));
                            let _ = capture_client.ReleaseBuffer(num_frames);
                            frame_count += num_frames as u64;
                        }
                    }

                    drop(capture_client);
                    if should_couninit {
                        CoUninitialize();
                    }
                }
            });

            audio_client.Start()
                .map_err(|_| WindowsAudioCaptureStreamCreateError::StreamStartFailed)?;

            Ok(WindowsAudioCaptureStream {
                should_couninit,
                audio_client,
                stop_flag,
                worker: Some(worker),
            })
        }
    }

    /// Stops the audio client and joins the worker thread (previously the worker looped forever).
    /// Idempotent.
    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        unsafe {
            let _ = self.audio_client.Stop();
        }
        if let Some(worker) = self.worker.take() {
            // Don't self-join if stop() is reached from the audio callback thread
            if worker.thread().id() != std::thread::current().id() {
                let _ = worker.join();
            }
        }
    }
}

impl Drop for WindowsAudioCaptureStream {
    fn drop(&mut self) {
        self.stop();
        unsafe {
            if self.should_couninit {
                CoUninitialize();
            }
        }
    }
}
