use std::{collections::HashMap, time::Duration};

use fiberplane::protocols::{
    names::Name,
    proxies::{SetDataSourcesMessage, UpsertProxyDataSource},
};

/// A token representing both:
/// - a task to check the status of the data source having a given name, and
/// - the retry strategy to use in case the status check failed.
#[derive(Debug, Clone)]
pub(crate) struct DataSourceCheckTask {
    name: Name,
    retries_left: isize,
    delay_till_next: Duration,
    backoff_factor: f32,
}

impl DataSourceCheckTask {
    /// Constructor
    ///
    /// `backoff_factor` MUST be greater than 1
    /// `initial_delay` MUST be greater than 0s
    ///
    /// The constructor guarantees that:
    /// - assuming a "try" takes a negligible amount of time,
    /// - no retry will be attempted past `total_checks_duration`
    ///
    /// If you are not sure that a "try" is going to be instant,
    /// you should add a safety buffer by decreasing the `total_checks_duration` argument.
    pub(crate) fn new(
        name: Name,
        total_checks_duration: Duration,
        initial_delay: Duration,
        backoff_factor: f32,
    ) -> DataSourceCheckTask {
        let max_retries: isize = if initial_delay > total_checks_duration {
            0
        } else {
            // Formula comes from the sum of terms in geometric series
            (((1.0
                + (backoff_factor - 1.0) * total_checks_duration.as_secs_f32()
                    / initial_delay.as_secs_f32())
            .ln()
                / backoff_factor.ln())
            .floor()
                - 1.0) as isize
        };

        Self {
            name,
            retries_left: max_retries,
            delay_till_next: initial_delay,
            backoff_factor,
        }
    }

    /// Return the next check task to accomplish after this one, with
    /// the delay to wait before sending it to the channel.
    pub(crate) fn next(self) -> Option<(Duration, Self)> {
        let Self {
            name,
            retries_left,
            delay_till_next,
            backoff_factor,
        } = self;
        if retries_left <= 0 {
            return None;
        }
        Some((
            delay_till_next,
            Self {
                name,
                retries_left: retries_left - 1,
                delay_till_next: Duration::from_secs_f32(
                    delay_till_next.as_secs_f32() * backoff_factor,
                ),
                backoff_factor,
            },
        ))
    }

    pub(crate) fn name(&self) -> &Name {
        &self.name
    }

    // Only used in tests
    #[allow(dead_code)]
    pub(crate) fn retries_left(&self) -> isize {
        self.retries_left
    }
}

/// The current state of a collection of data sources
#[derive(Debug, Clone, Default)]
pub(crate) struct DataSourcesStatusMap {
    inner: HashMap<Name, UpsertProxyDataSource>,
}

impl DataSourcesStatusMap {
    pub(crate) fn get_source_status(&self, name: &Name) -> Option<UpsertProxyDataSource> {
        self.inner.get(name).cloned()
    }

    /// Update the state of a single source.
    ///
    /// Returns the old state for this update, if it existed.
    pub(crate) fn update_source(
        &mut self,
        update: UpsertProxyDataSource,
    ) -> Option<UpsertProxyDataSource> {
        let name = update.name.clone();
        self.inner.insert(name, update)
    }

    /// Creates a ProxyMessage (the [SetDataSources](ProxyMessage::SetDataSources) variant) from
    /// the current state of the statuses.
    pub(crate) fn to_set_data_sources_message(&self) -> SetDataSourcesMessage {
        SetDataSourcesMessage {
            data_sources: self.inner.values().cloned().collect(),
        }
    }
}
