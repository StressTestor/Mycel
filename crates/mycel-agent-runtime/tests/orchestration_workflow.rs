use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use mycel_agent_runtime::{
    CancellationToken, CapabilitySet, ManifestPermissions, WorkerProfile, WorkflowArgValue,
    WorkflowManifest, WorkflowManifestStatus, WorkflowManifestStore, WorkflowPlan,
    WorkflowRunRequest, WorkflowRunner, WorkflowWorkerExecutor, WorkflowWorkerFuture,
    WorkflowWorkerRequest,
};

mod support;
use support::test_ports;

fn coder_profile() -> WorkerProfile {
    WorkerProfile {
        name: "coder".to_owned(),
        capabilities: CapabilitySet {
            tools: BTreeSet::from(["Read".to_owned(), "Edit".to_owned()]),
            filesystem_roots: BTreeSet::from(["/workspace".to_owned()]),
            network: false,
            can_spawn_subagents: false,
            can_swarm: false,
            can_workflow: false,
        },
        allow_delegation: false,
    }
}

fn plan_json() -> &'static str {
    r#"{
      "version": 1,
      "name": "release-review",
      "description": "review release",
      "phases": [
        {"title":"inspect","tasks":[
          {"id":"a","description":"a","prompt":"Inspect {{arg:release}}","worker_profile":"coder"},
          {"id":"b","description":"b","prompt":"Test {{arg:release}}","worker_profile":"coder"}
        ]},
        {"title":"synthesize","tasks":[
          {"id":"c","description":"c","prompt":"Combine {{result:a}} and {{result:b}}","worker_profile":"coder"}
        ]}
      ]
    }"#
}

#[test]
fn parser_rejects_recursion_forward_results_and_interpolation_smuggling() {
    let plan = WorkflowPlan::parse_json(plan_json()).expect("parse");
    let profiles = BTreeMap::from([("coder".to_owned(), coder_profile())]);
    let resolved = plan
        .resolve(
            &BTreeMap::from([(
                "release".to_owned(),
                WorkflowArgValue::String("v1".to_owned()),
            )]),
            &profiles,
            3,
        )
        .expect("resolve");
    assert_eq!(resolved.phases[0].tasks[0].prompt, "Inspect v1");

    assert!(plan
        .resolve(
            &BTreeMap::from([
                (
                    "release".to_owned(),
                    WorkflowArgValue::String("v1".to_owned()),
                ),
                ("unused".to_owned(), WorkflowArgValue::Bool(true)),
            ]),
            &profiles,
            3,
        )
        .is_err());
    assert!(plan
        .resolve(
            &BTreeMap::from([(
                "release".to_owned(),
                WorkflowArgValue::String("{{result:c}}".to_owned()),
            )]),
            &profiles,
            3,
        )
        .is_err());

    let recursive = WorkerProfile {
        allow_delegation: true,
        capabilities: CapabilitySet {
            can_spawn_subagents: true,
            ..coder_profile().capabilities
        },
        ..coder_profile()
    };
    assert!(plan
        .resolve(
            &BTreeMap::from([(
                "release".to_owned(),
                WorkflowArgValue::String("v1".to_owned()),
            )]),
            &BTreeMap::from([("coder".to_owned(), recursive)]),
            3,
        )
        .is_err());

    let mut four_workers: serde_json::Value =
        serde_json::from_str(plan_json()).expect("plan value");
    four_workers["phases"][1]["tasks"]
        .as_array_mut()
        .expect("tasks")
        .push(serde_json::json!({
            "id": "d",
            "description": "d",
            "prompt": "extra",
            "worker_profile": "coder"
        }));
    let four_workers = WorkflowPlan::parse_json(&four_workers.to_string()).expect("parse");
    assert!(four_workers
        .resolve(
            &BTreeMap::from([(
                "release".to_owned(),
                WorkflowArgValue::String("v1".to_owned()),
            )]),
            &profiles,
            3,
        )
        .is_err());

    let forward = plan_json().replace("Inspect {{arg:release}}", "Inspect {{result:c}}");
    assert!(WorkflowPlan::parse_json(&forward).is_err());
}

#[derive(Default)]
struct ManifestMemory {
    manifests: Mutex<BTreeMap<String, WorkflowManifest>>,
    permissions: Mutex<Vec<ManifestPermissions>>,
}

impl WorkflowManifestStore for ManifestMemory {
    fn load(&self) -> Result<Vec<WorkflowManifest>, String> {
        Ok(self
            .manifests
            .lock()
            .expect("manifests")
            .values()
            .cloned()
            .collect())
    }

    fn write(
        &self,
        manifest: &WorkflowManifest,
        permissions: ManifestPermissions,
    ) -> Result<(), String> {
        self.permissions
            .lock()
            .expect("permissions")
            .push(permissions);
        self.manifests
            .lock()
            .expect("manifests")
            .insert(manifest.run_id.clone(), manifest.clone());
        Ok(())
    }
}

struct OwnedParallelExecutor {
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<WorkflowWorkerRequest>>>,
}

impl WorkflowWorkerExecutor for OwnedParallelExecutor {
    fn execute(&self, request: WorkflowWorkerRequest) -> WorkflowWorkerFuture {
        self.requests
            .lock()
            .expect("requests")
            .push(request.clone());
        let active_counter = self.active.clone();
        let max_active = self.max_active.clone();
        Box::pin(async move {
            let active = active_counter.fetch_add(1, Ordering::SeqCst) + 1;
            max_active.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(10)).await;
            active_counter.fetch_sub(1, Ordering::SeqCst);
            Ok(format!("out:{}", request.task_id))
        })
    }
}

#[tokio::test]
async fn executor_runs_phases_sequentially_tasks_in_parallel_and_uses_manifest_permissions() {
    let plan = WorkflowPlan::parse_json(plan_json()).expect("parse");
    let resolved = plan
        .resolve(
            &BTreeMap::from([(
                "release".to_owned(),
                WorkflowArgValue::String("v1".to_owned()),
            )]),
            &BTreeMap::from([("coder".to_owned(), coder_profile())]),
            3,
        )
        .expect("resolve");
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let executor = Arc::new(OwnedParallelExecutor {
        active,
        max_active: max_active.clone(),
        requests: requests.clone(),
    });
    let manifests = Arc::new(ManifestMemory::default());
    let (ports, _store, _events) = test_ports(1_000);
    let runner = WorkflowRunner::new(ports, executor, manifests.clone());
    let manifest = runner
        .run(WorkflowRunRequest {
            run_id: "wf-00000000-0000-0000-0000-000000000001".to_owned(),
            plan: resolved,
            timeout: Duration::from_secs(1),
            cancellation: CancellationToken::new(),
        })
        .await
        .expect("run");
    assert_eq!(manifest.status, WorkflowManifestStatus::Completed);
    assert_eq!(max_active.load(Ordering::SeqCst), 2);
    let requests = requests.lock().expect("requests");
    assert_eq!(requests.len(), 3);
    assert!(requests[2].prompt.contains("out:a"));
    assert!(requests[2].prompt.contains("out:b"));
    assert!(manifests
        .permissions
        .lock()
        .expect("permissions")
        .iter()
        .all(|permissions| permissions.directory_mode == 0o700 && permissions.file_mode == 0o600));
}

struct PendingExecutor;

impl WorkflowWorkerExecutor for PendingExecutor {
    fn execute(&self, _request: WorkflowWorkerRequest) -> WorkflowWorkerFuture {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn timeout_and_restart_reconciliation_write_terminal_manifests() {
    let plan = WorkflowPlan::parse_json(plan_json()).expect("parse");
    let resolved = plan
        .resolve(
            &BTreeMap::from([(
                "release".to_owned(),
                WorkflowArgValue::String("v1".to_owned()),
            )]),
            &BTreeMap::from([("coder".to_owned(), coder_profile())]),
            3,
        )
        .expect("resolve");
    let manifests = Arc::new(ManifestMemory::default());
    let (ports, _store, _events) = test_ports(5_000);
    let runner = WorkflowRunner::new(ports, Arc::new(PendingExecutor), manifests.clone());
    let timed_out = runner
        .run(WorkflowRunRequest {
            run_id: "wf-00000000-0000-0000-0000-000000000002".to_owned(),
            plan: resolved,
            timeout: Duration::from_millis(2),
            cancellation: CancellationToken::new(),
        })
        .await
        .expect("timeout manifest");
    assert_eq!(timed_out.status, WorkflowManifestStatus::TimedOut);

    let mut lost_candidate = timed_out.clone();
    lost_candidate.run_id = "wf-00000000-0000-0000-0000-000000000003".to_owned();
    lost_candidate.status = WorkflowManifestStatus::Running;
    lost_candidate.ended_at_ms = None;
    manifests
        .write(&lost_candidate, ManifestPermissions::private())
        .expect("seed running");
    let lost = runner.reconcile_lost(&BTreeSet::new()).expect("lost");
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].status, WorkflowManifestStatus::Lost);
}

#[tokio::test]
async fn caller_cancellation_writes_an_aborted_terminal_manifest() {
    let plan = WorkflowPlan::parse_json(plan_json()).expect("parse");
    let resolved = plan
        .resolve(
            &BTreeMap::from([(
                "release".to_owned(),
                WorkflowArgValue::String("v1".to_owned()),
            )]),
            &BTreeMap::from([("coder".to_owned(), coder_profile())]),
            3,
        )
        .expect("resolve");
    let manifests = Arc::new(ManifestMemory::default());
    let (ports, _store, _events) = test_ports(5_000);
    let runner = WorkflowRunner::new(ports, Arc::new(PendingExecutor), manifests);
    let cancellation = CancellationToken::new();
    let cancel_from_task = cancellation.clone();
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        cancel_from_task.cancel();
    });
    let aborted = runner
        .run(WorkflowRunRequest {
            run_id: "wf-00000000-0000-0000-0000-000000000004".to_owned(),
            plan: resolved,
            timeout: Duration::from_secs(1),
            cancellation,
        })
        .await
        .expect("aborted manifest");
    assert_eq!(aborted.status, WorkflowManifestStatus::Aborted);
    assert_eq!(aborted.error.as_deref(), Some("workflow cancelled"));
}
