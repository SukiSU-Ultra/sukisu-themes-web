use std::sync::{Arc, Mutex};

use crate::models::JobState;
use crate::util::{now_iso, random_hex};

#[derive(Clone, Default)]
pub struct JobManager {
    state: Arc<Mutex<JobState>>,
}

#[derive(Clone)]
pub struct JobProgress {
    state: Arc<Mutex<JobState>>,
}

impl JobManager {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(JobState::default())),
        }
    }

    pub fn snapshot(&self) -> JobState {
        self.state.lock().unwrap().clone()
    }

    pub fn is_running(&self) -> bool {
        self.state.lock().unwrap().running
    }

    pub fn start(&self, kind: &str) -> Option<JobProgress> {
        let mut state = self.state.lock().unwrap();
        if state.running {
            return None;
        }
        *state = JobState {
            id: random_hex(8),
            kind: kind.to_string(),
            running: true,
            done: false,
            error: None,
            message: "准备中".to_string(),
            progress: 0,
            total: 0,
            started_at: now_iso(),
        };
        Some(JobProgress {
            state: self.state.clone(),
        })
    }
}

impl JobProgress {
    pub fn set_total(&self, total: u64) {
        if let Ok(mut state) = self.state.lock() {
            state.total = total;
        }
    }

    pub fn set_progress(&self, progress: u64) {
        if let Ok(mut state) = self.state.lock() {
            state.progress = progress;
        }
    }

    pub fn add_progress(&self, bytes: u64) {
        if let Ok(mut state) = self.state.lock() {
            state.progress = state.progress.saturating_add(bytes);
        }
    }

    pub fn set_message(&self, message: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.message = message.to_string();
        }
    }

    pub fn finish_ok(&self, message: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.running = false;
            state.done = true;
            state.error = None;
            state.message = message.to_string();
        }
    }

    pub fn finish_err(&self, message: &str, error: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.running = false;
            state.done = false;
            state.error = Some(error.to_string());
            state.message = message.to_string();
        }
    }

    pub fn fail(&self, error: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.running = false;
            state.done = false;
            state.error = Some(error.to_string());
            state.message = "迁移失败".to_string();
        }
    }
}
