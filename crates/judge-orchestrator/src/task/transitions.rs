//! Which state may follow which. The table is the whole policy: nothing else moves a task.

use host_core::TaskState;

/// Whether a task in `from` may move to `to`. `Paused` is left only for the state it was paused
/// from (or to stop the task), which the repository checks with `paused_from`.
#[must_use]
pub fn is_allowed(from: TaskState, to: TaskState) -> bool {
    use TaskState::{
        Applied, AwaitingReview, Cancelled, ContextReady, Created, Failed, NeedsReconciliation,
        Paused, Planned, Running, Verifying,
    };
    if from == to || from.is_terminal() {
        return false;
    }
    // Every live task can be stopped.
    if matches!(to, Cancelled | Failed) {
        return true;
    }
    match from {
        Created => matches!(to, ContextReady),
        ContextReady => matches!(to, Planned | Paused),
        Planned => matches!(to, Running | Paused),
        // External actions happen while running or verifying: an interrupted one needs a human.
        Running => matches!(to, Verifying | Paused | NeedsReconciliation),
        // Back to `Running` is an automatic correction cycle (bounded by the budget).
        Verifying => matches!(to, AwaitingReview | Running | Paused | NeedsReconciliation),
        AwaitingReview => matches!(to, Applied | Running),
        Paused => matches!(to, ContextReady | Planned | Running | Verifying),
        // Once a human has settled every interrupted action, work resumes.
        NeedsReconciliation => matches!(to, Running),
        Applied | Failed | Cancelled => false,
    }
}

#[cfg(test)]
mod tests {
    use super::is_allowed;
    use host_core::TaskState::{self, *};

    #[test]
    fn the_happy_path_is_allowed_step_by_step() {
        let path = [
            Created,
            ContextReady,
            Planned,
            Running,
            Verifying,
            AwaitingReview,
            Applied,
        ];
        for pair in path.windows(2) {
            assert!(
                is_allowed(pair[0], pair[1]),
                "{:?} -> {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn steps_cannot_be_skipped() {
        assert!(!is_allowed(Created, Running));
        assert!(!is_allowed(Planned, Applied));
        assert!(!is_allowed(Running, Applied));
        assert!(!is_allowed(Verifying, Applied));
    }

    #[test]
    fn terminal_states_are_final() {
        for from in [Applied, Failed, Cancelled] {
            for to in TaskState::ALL {
                assert!(!is_allowed(from, to), "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn every_live_task_can_be_stopped() {
        for from in TaskState::ALL
            .into_iter()
            .filter(|state| !state.is_terminal())
        {
            assert!(is_allowed(from, Cancelled), "{from:?}");
            assert!(is_allowed(from, Failed), "{from:?}");
        }
    }

    #[test]
    fn reconciliation_only_resumes_work() {
        assert!(is_allowed(Running, NeedsReconciliation));
        assert!(is_allowed(NeedsReconciliation, Running));
        assert!(!is_allowed(NeedsReconciliation, Verifying));
        assert!(!is_allowed(NeedsReconciliation, Applied));
        assert!(!is_allowed(Created, NeedsReconciliation));
    }
}
