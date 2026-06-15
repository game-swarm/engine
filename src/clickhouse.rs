use std::{sync::mpsc, thread, time::Duration};

use crate::tick::{CLICKHOUSE_TICK_METRICS_INSERT, ClickHouseTickMetricsRow};

const DEFAULT_QUEUE_CAPACITY: usize = 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct ClickHouseWriter {
    sender: mpsc::SyncSender<ClickHouseTickMetricsRow>,
}

impl ClickHouseWriter {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_queue_capacity(base_url, DEFAULT_QUEUE_CAPACITY)
    }

    pub fn with_queue_capacity(base_url: impl Into<String>, queue_capacity: usize) -> Self {
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let endpoint = format!("{}/", base_url.into().trim_end_matches('/'));

        thread::spawn(move || {
            let client = match reqwest::blocking::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
            {
                Ok(client) => client,
                Err(_) => return,
            };

            for row in receiver {
                let _ = client
                    .post(&endpoint)
                    .query(&[("query", CLICKHOUSE_TICK_METRICS_INSERT)])
                    .body(Self::insert_body(&row))
                    .send()
                    .and_then(|response| response.error_for_status())
                    .map(|_| ());
            }
        });

        Self { sender }
    }

    pub fn enqueue(&self, row: ClickHouseTickMetricsRow) {
        let _ = self.sender.try_send(row);
    }

    pub fn insert_body(row: &ClickHouseTickMetricsRow) -> String {
        row.insert_sql_values()
    }
}
