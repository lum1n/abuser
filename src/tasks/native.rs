//! abuser's own queue, filled with `abuser add`.

use anyhow::Result;

use super::{Outcome, Source, Task};
use crate::repos::Repo;
use crate::store::{JobStatus, Store};

pub struct Native {
    store: Store,
}

impl Native {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Source for Native {
    fn name(&self) -> &str {
        "native"
    }

    async fn list(&self, _repos: &[Repo]) -> Result<Vec<Task>> {
        Ok(self
            .store
            .open_native()?
            .into_iter()
            .map(|t| Task {
                source: "native".to_owned(),
                id: t.id.to_string(),
                title: t.title,
                body: t.body,
                repo: Some(t.repo),
                meta: Default::default(),
            })
            .collect())
    }

    async fn complete(&self, task: &Task, _repo: &Repo, _outcome: &Outcome) -> Result<()> {
        if let Ok(id) = task.id.parse() {
            self.store.close_native(id)?;
        }
        Ok(())
    }

    async fn fail(&self, task: &Task, _repo: &Repo, outcome: &Outcome) -> Result<()> {
        // A task the agent says can't be done won't get better by retrying.
        if outcome.status == JobStatus::Blocked
            && let Ok(id) = task.id.parse()
        {
            self.store.close_native(id)?;
        }
        Ok(())
    }
}
