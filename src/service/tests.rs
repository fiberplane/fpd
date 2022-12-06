use crate::service::status_check::DataSourceCheckTask;
use fiberplane::models::names::Name;
use std::time::Duration;

#[test]
fn exponential_backoff_cap() {
    fn test_rec(task: DataSourceCheckTask, old_delay: Duration, remaining_budget: Duration) {
        if let Some((delay, new_task)) = task.next() {
            assert!(
                delay > old_delay,
                "the new delay is longer than the old one."
            );
            let new_remaining_budget = remaining_budget.checked_sub(delay);
            assert!(
                new_remaining_budget.is_some(),
                "The delay ({:?}) is bigger than the remaining budget {:?}",
                delay,
                remaining_budget
            );
            test_rec(new_task, delay, new_remaining_budget.unwrap());
        }
    }

    fn test_case(total_duration: Duration, initial_delay: Duration, backoff_factor: f32) {
        let task = DataSourceCheckTask::new(
            Name::from_static("be-the-change"),
            total_duration,
            initial_delay,
            backoff_factor,
        );
        assert!(task.retries_left() >= 0, "At least 1 try will be attempted");
        test_rec(task, Duration::from_secs(0), total_duration);
    }

    test_case(Duration::from_secs(300), Duration::from_secs(10), 1.5);
    test_case(Duration::from_secs(300), Duration::from_secs(1000), 1.5);
    test_case(Duration::from_secs(300), Duration::from_secs(300), 1.5);
}
