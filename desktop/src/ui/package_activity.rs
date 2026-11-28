//! Tracks one atomic package operation at a time and keeps its outcome on the initiating card.
use crate::plugin::packages::{Operation, PackageStage};
use std::collections::HashMap;
use uuid::Uuid;

pub(super) enum Status {
    Running(PackageStage),
    Completed,
    Failed(String),
}

pub(super) struct Job {
    pub operation: Operation,
    pub status: Status,
}

#[derive(Default)]
pub(crate) struct PackageActivity {
    active: Option<(Uuid, Job)>,
    plugins: HashMap<String, Job>,
    install: Option<Job>,
}

impl PackageActivity {
    pub fn busy(&self) -> bool {
        self.active.is_some()
    }

    pub(super) fn start(&mut self, operation: Operation) -> Option<Uuid> {
        if self.busy() {
            return None;
        }
        let id = Uuid::new_v4();
        self.active = Some((
            id,
            Job {
                operation,
                status: Status::Running(PackageStage::Preparing),
            },
        ));
        Some(id)
    }

    pub(super) fn advance(&mut self, id: Uuid, stage: PackageStage) {
        if let Some((_, job)) = self.active.as_mut().filter(|(current, _)| *current == id) {
            job.status = Status::Running(stage);
        }
    }

    pub(super) fn finish(&mut self, id: Uuid, result: &Result<String, String>) -> bool {
        let Some((_, mut job)) = self.active.take_if(|(current, _)| *current == id) else {
            return false;
        };
        job.status = match result {
            Ok(_) => Status::Completed,
            Err(err) => Status::Failed(err.clone()),
        };
        match &job.operation {
            Operation::Install(_) => self.install = Some(job),
            Operation::Remove(id) if result.is_ok() => {
                self.plugins.remove(id);
            }
            Operation::Update(id) | Operation::Remove(id) => {
                self.plugins.insert(id.clone(), job);
            }
        }
        true
    }

    pub(super) fn plugin(&self, id: &str) -> Option<&Job> {
        if let Some((_, job)) = &self.active {
            if matches!(&job.operation, Operation::Update(target) | Operation::Remove(target) if target == id)
            {
                return Some(job);
            }
        }
        self.plugins.get(id)
    }

    pub(super) fn install(&self) -> Option<&Job> {
        if let Some((_, job)) = &self.active {
            if matches!(job.operation, Operation::Install(_)) {
                return Some(job);
            }
        }
        self.install.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_update_can_retry_without_late_events_overwriting_it() {
        let mut state = PackageActivity::default();
        let op = Operation::Update("example".into());
        let first = state.start(op.clone()).unwrap();
        assert!(state.start(Operation::Remove("other".into())).is_none());
        state.advance(first, PackageStage::Resolving);
        assert!(state.finish(first, &Err("offline".into())));
        assert!(
            matches!(&state.plugin("example").unwrap().status, Status::Failed(err) if err == "offline")
        );
        let second = state.start(op).unwrap();
        state.advance(first, PackageStage::Activating);
        assert!(!state.finish(first, &Ok("example".into())));
        assert!(matches!(
            state.plugin("example").unwrap().status,
            Status::Running(PackageStage::Preparing)
        ));
        assert!(state.finish(second, &Ok("example".into())));
        assert!(!state.busy());
        assert!(matches!(
            state.plugin("example").unwrap().status,
            Status::Completed
        ));
    }
}
