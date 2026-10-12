//! What an engine process does, from `ENGINE_ROLES` (comma-separated):
//!   - `api`: serve the HTTP API. Webhook calls run in this process unless queued
//!     (`?async=true`); signals resume waiting runs here.
//!   - `worker`: take queued runs (see [`crate::run_queue`]) and resume runs whose wait timer
//!     is due (see [`crate::waits`]).
//!   - `scheduler`: fire due schedules, queueing their runs for workers (see
//!     [`crate::scheduler`]).
//!
//! The default, `api,worker,scheduler`, is one process doing everything. At scale, run
//! separate deployments sharing one database and scale each on its own: API and workers by
//! load, schedulers usually one or two (more are safe, not faster).

pub const DEFAULT_ROLES: &str = "api,worker,scheduler";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Roles {
    pub api: bool,
    pub worker: bool,
    pub scheduler: bool,
}

impl Roles {
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut roles = Self { api: false, worker: false, scheduler: false };
        for role in s.split(',').map(str::trim).filter(|r| !r.is_empty()) {
            match role {
                "api" => roles.api = true,
                "worker" => roles.worker = true,
                "scheduler" => roles.scheduler = true,
                other => {
                    return Err(format!("unknown engine role '{other}': expected api, worker or scheduler"))
                }
            }
        }
        if !roles.api && !roles.worker && !roles.scheduler {
            return Err("ENGINE_ROLES must name at least one role: api, worker, scheduler".to_string());
        }
        Ok(roles)
    }

    pub fn names(self) -> String {
        [("api", self.api), ("worker", self.worker), ("scheduler", self.scheduler)]
            .iter()
            .filter(|(_, on)| *on)
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_everything() {
        assert_eq!(Roles::parse(DEFAULT_ROLES), Ok(Roles { api: true, worker: true, scheduler: true }));
    }

    #[test]
    fn parses_single_roles_with_spaces() {
        assert_eq!(Roles::parse(" worker "), Ok(Roles { api: false, worker: true, scheduler: false }));
        assert_eq!(Roles::parse("api,"), Ok(Roles { api: true, worker: false, scheduler: false }));
        assert_eq!(Roles::parse("scheduler"), Ok(Roles { api: false, worker: false, scheduler: true }));
    }

    #[test]
    fn rejects_unknown_or_empty() {
        assert!(Roles::parse("api,cron").is_err());
        assert!(Roles::parse(" , ").is_err());
    }

    #[test]
    fn names_round_trip() {
        assert_eq!(Roles::parse("scheduler,worker,api").unwrap().names(), "api,worker,scheduler");
    }
}
