use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    InvalidInput,
    NotFound,
    ScopeMismatch,
    RevisionConflict,
    RequestKeyConflict,
    CapabilityUnavailable,
    InstanceChanged,
    NotReady,
    ApprovalRequired,
    DeliveryUnconfirmed,
    ArtifactChanged,
    ArtifactMissing,
    InputExpired,
    ResourceLimit,
    StorageUnavailable,
}
#[derive(Debug)]
pub struct WorkError(pub FailureCode);
impl std::fmt::Display for WorkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}
impl std::error::Error for WorkError {}
impl From<rusqlite::Error> for WorkError {
    fn from(_: rusqlite::Error) -> Self {
        Self(FailureCode::StorageUnavailable)
    }
}
impl From<serde_json::Error> for WorkError {
    fn from(_: serde_json::Error) -> Self {
        Self(FailureCode::StorageUnavailable)
    }
}
pub type WorkResult<T> = Result<T, WorkError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskPolicy {
    pub allowed_agents: Vec<String>,
    pub max_workers: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTask {
    pub repo_path: String,
    pub title: String,
    pub brief: String,
    pub parent_task_id: Option<String>,
    pub policy: TaskPolicy,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    #[serde(default)]
    pub delegation: DelegationState,
    #[serde(default)]
    pub dependencies: Vec<TaskDependency>,
    #[serde(default)]
    pub input_refs: Vec<FrozenInputRef>,
    pub id: String,
    pub session_id: String,
    pub repo_path: String,
    pub title: String,
    pub brief: String,
    pub parent_task_id: Option<String>,
    pub revision: u64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub paused: bool,
    pub policy: TaskPolicy,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptRole {
    Lead,
    Worker,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewAttempt {
    pub agent_kind: String,
    pub role: AttemptRole,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PauseInput {
    pub paused: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeBinding {
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    pub instance_id: Option<String>,
    pub target: Option<String>,
    pub pane_id: Option<String>,
    pub worktree_path: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    #[serde(default)]
    pub lifecycle: AttemptLifecycle,
    pub id: String,
    pub task_id: String,
    pub agent_kind: String,
    pub role: AttemptRole,
    #[serde(flatten)]
    pub binding: NativeBinding,
    pub created_at_ms: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    CreateTask,
    StartAttempt,
    DeliverPrompt,
    InterruptAttempt,
    SubmitResult,
    ReviewResult,
    PauseTask,
    ReconcileAttempt,
    ConfigureDelegation,
    SetDependencies,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Prepared,
    Submitting,
    Acknowledged,
    Refused,
    Unconfirmed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    #[serde(default)]
    pub interruption_receipt: Option<InterruptionReceipt>,
    #[serde(default)]
    pub interruption_owner_epoch: Option<String>,
    #[serde(default)]
    pub delegation_fence: Option<DelegationFence>,
    #[serde(default)]
    pub dependency_snapshot: Vec<DependencySnapshot>,
    #[serde(default)]
    pub input_refs: Vec<FrozenInputRef>,
    #[serde(default)]
    pub bootstrap_version: Option<String>,
    pub id: String,
    pub task_id: String,
    pub attempt_id: Option<String>,
    pub kind: OperationKind,
    pub state: OperationState,
    pub resources: NativeBinding,
    pub failure_code: Option<FailureCode>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}
#[derive(Debug, Clone)]
pub struct OperationOutcome {
    pub state: OperationState,
    pub resources: NativeBinding,
    pub failure_code: Option<FailureCode>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub path: String,
    pub sha256: String,
    pub size_bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultInput {
    pub attempt_id: String,
    pub summary: String,
    pub artifacts: Vec<ArtifactRef>,
    pub evidence: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultSubmission {
    pub id: String,
    pub task_id: String,
    #[serde(flatten)]
    pub result: ResultInput,
    pub created_at_ms: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Accepted,
    ChangesRequested,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewInput {
    pub submission_id: String,
    pub decision: ReviewDecision,
    pub message: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub id: String,
    pub task_id: String,
    pub actor_id: String,
    #[serde(flatten)]
    pub review: ReviewInput,
    pub created_at_ms: i64,
}
/// Read-only reconciliation of one authenticated actor's committed request key.
#[derive(Debug, Clone, Serialize)]
pub struct RequestReceipt {
    pub kind: OperationKind,
    pub value: serde_json::Value,
}
#[derive(Debug, Clone, Serialize)]
pub struct Mutation<T> {
    pub value: T,
    pub replayed: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct TaskDetail {
    pub task: Task,
    pub attempts: Vec<Attempt>,
    pub operations: Vec<Operation>,
    pub results: Vec<ResultSubmission>,
    pub reviews: Vec<Review>,
    pub cursor: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskChange {
    pub cursor: u64,
    pub task_id: String,
    pub revision: u64,
    pub kind: String,
    pub entity_id: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct ChangePage {
    pub changes: Vec<TaskChange>,
    pub cursor: u64,
    pub reset_required: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchPhase {
    NotDispatched,
    DispatchClaimed,
    LaunchConfirmed,
    #[default]
    LegacyUnknown,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reservation {
    #[default]
    Reserved,
    Released,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AttemptLifecycle {
    pub launch_phase: LaunchPhase,
    pub reservation: Reservation,
    pub native_owner_epoch: Option<String>,
    pub release: Option<AttemptRelease>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRelease {
    pub reason: ReleaseReason,
    pub evidence: ReleaseEvidence,
    pub reconciliation_operation_id: String,
    pub released_at_ms: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason {
    StartupNotDispatched,
    StartupRefusedWithoutProcess,
    OwnedProcessExited,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReleaseEvidence {
    GatewayDispatchFence {
        start_operation_id: String,
    },
    NativeStartRefusal {
        start_operation_id: String,
        native_owner_epoch: String,
        native_receipt_id: String,
    },
    NativeExitTombstone {
        instance_id: String,
        native_owner_epoch: String,
        native_receipt_id: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconcileInput {
    pub request_key: String,
    pub expected_revision: u64,
    #[serde(deserialize_with = "required_nullable_identity")]
    pub expected_instance_id: Option<String>,
    #[serde(deserialize_with = "required_nullable_identity")]
    pub expected_native_owner_epoch: Option<String>,
}
fn required_nullable_identity<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}
/// Trusted service evidence, never deserialize directly from an HTTP request.
#[derive(Debug, Clone)]
pub enum ReconciliationEvidence {
    NotDispatched,
    NativeNotStarted {
        start_operation_id: String,
        owner_epoch: String,
        receipt_id: String,
    },
    Live {
        instance_id: String,
        owner_epoch: String,
    },
    Exited {
        instance_id: String,
        owner_epoch: String,
        receipt_id: String,
    },
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleObservation {
    NotStarted,
    Live,
    Exited,
    Unknown,
    AlreadyReleased,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationReceipt {
    pub operation_id: String,
    pub attempt_id: String,
    pub observation: LifecycleObservation,
    pub reservation: Reservation,
    pub release: Option<AttemptRelease>,
    pub task_revision: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeStartRefusal {
    pub start_operation_id: String,
    pub owner_epoch: String,
    pub receipt_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputUpload {
    pub repo_path: String,
    pub name: String,
    pub mime: String,
    pub size_bytes: u64,
    pub sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputReceipt {
    pub input_id: String,
    pub session_id: String,
    pub repo_path: String,
    pub name: String,
    pub mime: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum InputUse {
    #[default]
    #[serde(rename = "reference-only")]
    ReferenceOnly,
    #[serde(rename = "may-include")]
    MayInclude,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputRef {
    pub input_id: String,
    #[serde(default)]
    pub caption: String,
    #[serde(default, rename = "use")]
    pub use_: InputUse,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FrozenInputRef {
    pub input_id: String,
    pub caption: String,
    #[serde(rename = "use")]
    pub use_: InputUse,
    pub name: String,
    pub mime: String,
    pub size_bytes: u64,
    pub sha256: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DependencyRequirement {
    #[default]
    ResultAvailable,
    HumanAccepted,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DelegationPolicy {
    pub enabled: bool,
    pub max_children: u32,
    pub max_depth: u32,
    pub dependency_requirement: DependencyRequirement,
}
impl Default for DelegationPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_children: 16,
            max_depth: 1,
            dependency_requirement: DependencyRequirement::ResultAvailable,
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DelegationState {
    pub policy: DelegationPolicy,
    pub coordinator_attempt_id: Option<String>,
    pub coordinator_epoch: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationConfig {
    pub policy: DelegationPolicy,
    pub coordinator_attempt_id: Option<String>,
}
/// Constructed from authenticated local authority, never accepted as caller-supplied authority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DelegationFence {
    pub coordinator_task_id: String,
    pub coordinator_attempt_id: String,
    pub coordinator_epoch: u64,
    pub instance_id: String,
    pub native_owner_epoch: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskDependency {
    pub prerequisite_task_id: String,
    pub submission_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DependencySnapshot {
    pub prerequisite_task_id: String,
    pub submission_id: String,
    pub review_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DependencyReadiness {
    Ready,
    WaitingForResult,
    WaitingForAcceptance,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputShare {
    pub id: String,
    pub source_task_id: String,
    pub recipient_task_id: String,
    pub input_id: String,
    pub source_use: InputUse,
    pub permitted_use: InputUse,
    pub sha256: String,
    pub size_bytes: u64,
    pub name: String,
    pub mime: String,
    pub coordinator_attempt_id: String,
    pub coordinator_epoch: u64,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterruptInput {
    pub request_key: String,
    pub expected_revision: u64,
    pub expected_instance_id: String,
    pub expected_native_owner_epoch: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InterruptionReceipt {
    pub operation_id: String,
    pub launch_id: String,
    pub owner_epoch: String,
    pub receipt_id: String,
    pub key: String,
    pub bytes_written: u64,
    pub input_disposition: String,
}
#[derive(Debug, Clone)]
pub enum InterruptionOutcome {
    Acknowledged(InterruptionReceipt),
    Refused(FailureCode),
    Unconfirmed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskActivity {
    pub cursor: u64,
    pub kind: String,
    pub entity_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SummaryReview {
    pub review_id: String,
    pub decision: ReviewDecision,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SummaryResult {
    pub submission_id: String,
    pub review: Option<SummaryReview>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskSummary {
    pub task_id: String,
    pub session_id: String,
    pub parent_task_id: Option<String>,
    pub task_revision: u64,
    pub title: String,
    pub repo_path: String,
    pub paused: bool,
    pub last_activity: Option<TaskActivity>,
    pub reserved_attempts: u64,
    pub unresolved_native_operations: u64,
    pub unreviewed_results: u64,
    pub latest_result: Option<SummaryResult>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskSummaryPage {
    pub items: Vec<TaskSummary>,
    pub snapshot_cursor: u64,
    pub next_after_id: Option<String>,
}
