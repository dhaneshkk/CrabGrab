/-
  Formal model of CrabGrab's Windows capture-stream stop flag.

  Source: src/platform/windows/capture_stream.rs (`WindowsCaptureStream::stop`, the `Closed`
  handler and the `FrameArrived` handler).

  Model. The stream's `closed : AtomicBool` starts `false`. Three event kinds occur in any order:
    * `stop`   - `stop()`, including the call made from `Drop`
    * `closed` - the WinRT `GraphicsCaptureItem.Closed` event (target window/display went away)
    * `frame`  - `FrameArrived`
  After the fix, every handler takes the callback mutex *before* touching the flag and keeps it
  while it emits, so events are serialised and each one is a single atomic step here.

  `stop`/`closed` do `old := flag.rmw(true)` and emit `End` iff `old = false`.
  `frame` emits a video frame iff the flag (read under the lock) is `false`.

  Results:
    * `fixed_closed_is_silent`, `fixed_output_shape`: with `fetch_or`, the output of ANY event
      trace is `frame^k` followed by at most one `End` - so End is delivered at most once,
      exactly when a stop/close happened, and no frame is ever delivered after End.
    * `buggy_flag_never_set`, `buggy_end_count`, `buggy_*` examples: with the original
      `fetch_and(true)`, the flag is never set; End fires once per stop/close (e.g. twice for
      `stop()` then `drop`) and frames keep flowing after End.

  Check with:  lean formal/StopFlag.lean   (Lean 4 core only, no Mathlib)
-/

namespace CrabGrab.StopFlag

inductive Ev where
  | stop
  | closed
  | frame
  deriving DecidableEq, Repr

inductive Out where
  | endEv
  | frameEv
  deriving DecidableEq, Repr

/-- An atomic read-modify-write with operand `true`: returns (previous value, new value). -/
abbrev Rmw := Bool → Bool × Bool

/-- `AtomicBool::fetch_or(true)` - the fix. -/
def fetchOr : Rmw := fun b => (b, b || true)

/-- `AtomicBool::fetch_and(true)` - the original code. -/
def fetchAnd : Rmw := fun b => (b, b && true)

def isStopLike : Ev → Bool
  | .stop => true
  | .closed => true
  | .frame => false

/-- One serialised handler invocation: (new flag, outputs delivered to the user callback). -/
def step (rmw : Rmw) (flag : Bool) : Ev → Bool × List Out
  | .stop | .closed =>
    let r := rmw flag
    (r.2, if r.1 then [] else [.endEv])
  | .frame => (flag, if flag then [] else [.frameEv])

/-- Run a whole trace, concatenating everything delivered to the callback. -/
def run (rmw : Rmw) : Bool → List Ev → Bool × List Out
  | flag, [] => (flag, [])
  | flag, e :: es =>
    let r := step rmw flag e
    let rest := run rmw r.1 es
    (rest.1, r.2 ++ rest.2)

/-- Frames that arrive before the first stop/close. -/
def framesBeforeStop : List Ev → Nat
  | [] => 0
  | .frame :: es => framesBeforeStop es + 1
  | .stop :: _ => 0
  | .closed :: _ => 0

def countEnd (l : List Out) : Nat := (l.filter (· == .endEv)).length

/-! ### The fix: `fetch_or` -/

/-- Once closed, nothing at all is delivered, whatever happens next. -/
theorem fixed_closed_is_silent (es : List Ev) : run fetchOr true es = (true, []) := by
  induction es with
  | nil => rfl
  | cons e es ih => cases e <;> simp [run, step, fetchOr, ih]

/-- The complete output of any trace: some frames, then End exactly once iff a stop/close
    occurred. In particular End is never duplicated and no frame follows End. -/
theorem fixed_output_shape (es : List Ev) :
    (run fetchOr false es).2 =
      List.replicate (framesBeforeStop es) .frameEv ++ (if es.any isStopLike then [.endEv] else []) := by
  induction es with
  | nil => rfl
  | cons e es ih =>
    cases e with
    | stop => simp [run, step, fetchOr, fixed_closed_is_silent, framesBeforeStop, isStopLike]
    | closed => simp [run, step, fetchOr, fixed_closed_is_silent, framesBeforeStop, isStopLike]
    | frame =>
      simp only [run, step, fetchOr, framesBeforeStop, List.any_cons, isStopLike, Bool.false_or]
      simp only [Bool.false_eq_true, ↓reduceIte] at *
      rw [ih, List.replicate_succ]
      simp

theorem fixed_end_at_most_once (es : List Ev) : countEnd (run fetchOr false es).2 ≤ 1 := by
  rw [fixed_output_shape]
  have h : countEnd (List.replicate (framesBeforeStop es) Out.frameEv) = 0 := by
    induction (framesBeforeStop es) with
    | zero => rfl
    | succ n ih => simp_all [countEnd, List.replicate_succ]
  cases es.any isStopLike <;> simp_all [countEnd, List.filter_append]

theorem fixed_end_iff_stop (es : List Ev) :
    Out.endEv ∈ (run fetchOr false es).2 ↔ es.any isStopLike = true := by
  rw [fixed_output_shape]
  cases h : es.any isStopLike <;> simp [List.mem_replicate]

/-- `true` iff no `frameEv` occurs anywhere after an `endEv`. -/
def noFrameAfterEnd : List Out → Bool
  | [] => true
  | .endEv :: rest => !(rest.contains .frameEv)
  | .frameEv :: rest => noFrameAfterEnd rest

theorem noFrameAfterEnd_split (pre post : List Out) :
    noFrameAfterEnd (pre ++ Out.endEv :: post) = true → Out.frameEv ∉ post := by
  induction pre with
  | nil => intro h; simpa [noFrameAfterEnd] using h
  | cons x pre ih =>
    cases x with
    | frameEv => intro h; exact ih (by simpa [noFrameAfterEnd] using h)
    | endEv =>
      intro h hmem
      simp [noFrameAfterEnd] at h
      exact h.2 hmem

theorem fixed_output_noFrameAfterEnd (es : List Ev) :
    noFrameAfterEnd (run fetchOr false es).2 = true := by
  rw [fixed_output_shape]
  induction (framesBeforeStop es) with
  | zero => cases es.any isStopLike <;> rfl
  | succ n ih => simpa [List.replicate_succ, noFrameAfterEnd] using ih

/-- No video frame is delivered after End: wherever End appears in the output, no frame follows. -/
theorem fixed_no_frame_after_end (es : List Ev) (pre post : List Out) :
    (run fetchOr false es).2 = pre ++ Out.endEv :: post → Out.frameEv ∉ post := by
  intro h
  apply noFrameAfterEnd_split pre post
  rw [← h]
  exact fixed_output_noFrameAfterEnd es

/-! ### The bug: `fetch_and` -/

/-- With `fetch_and(true)` the flag can never become true. -/
theorem buggy_flag_never_set (es : List Ev) : (run fetchAnd false es).1 = false := by
  induction es with
  | nil => rfl
  | cons e es ih => cases e <;> simp [run, step, fetchAnd, ih]

/-- ... so End is delivered once per stop/close instead of once in total. -/
theorem buggy_end_count (es : List Ev) :
    countEnd (run fetchAnd false es).2 = (es.filter isStopLike).length := by
  induction es with
  | nil => rfl
  | cons e es ih =>
    cases e <;> simp_all [run, step, fetchAnd, countEnd, List.filter_append, isStopLike, List.filter_cons]
    all_goals omega

/-- `stream.stop()` followed by `drop(stream)`: the user saw End twice. -/
example : (run fetchAnd false [.stop, .stop]).2 = [.endEv, .endEv] := by decide

/-- A frame arriving after `stop()` was still delivered. -/
example : (run fetchAnd false [.stop, .frame]).2 = [.endEv, .frameEv] := by decide
example : noFrameAfterEnd (run fetchAnd false [.stop, .frame]).2 = false := by decide

/-- The same traces under the fix. -/
example : (run fetchOr false [.stop, .stop]).2 = [.endEv] := by decide
example : (run fetchOr false [.stop, .frame]).2 = [.endEv] := by decide

end CrabGrab.StopFlag

#print axioms CrabGrab.StopFlag.fixed_output_shape
#print axioms CrabGrab.StopFlag.fixed_end_at_most_once
#print axioms CrabGrab.StopFlag.fixed_no_frame_after_end
#print axioms CrabGrab.StopFlag.buggy_end_count
