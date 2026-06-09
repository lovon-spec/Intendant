//! Serde model for the station snapshot: the wire schema that
//! `static/app.html` feeds through `update_snapshot`. Kept schema-complete
//! even where the renderer only reads a subset of fields.

use serde::{Deserialize, Deserializer};

#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct StationSnapshot {
    pub(crate) hosts: Vec<StationHost>,
    pub(crate) agents: Vec<StationAgent>,
    pub(crate) events: Vec<StationEvent>,
    pub(crate) activity: StationActivitySummary,
    pub(crate) context: StationContextSummary,
    pub(crate) managed: StationManagedSummary,
    pub(crate) changes: StationChangesSummary,
    pub(crate) sessions: StationSessionsSummary,
    pub(crate) controls: StationControlsSummary,
    pub(crate) attention_queue: StationAttentionQueueSummary,
    pub(crate) display_runway: StationDisplayRunwaySummary,
}

#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct StationActivitySummary {
    pub(crate) retained_count: usize,
    pub(crate) shown_count: usize,
    pub(crate) managed_count: usize,
    pub(crate) thread_count: usize,
    pub(crate) host_filter: String,
    pub(crate) level_filter: String,
    pub(crate) source_filter: String,
    pub(crate) query: String,
    pub(crate) verbosity: String,
    pub(crate) latest_id: String,
    pub(crate) latest_level: String,
    pub(crate) latest_source: String,
    pub(crate) latest_host: String,
    pub(crate) latest_session_id: String,
    pub(crate) latest_text: String,
    pub(crate) top_levels: String,
    pub(crate) top_sources: String,
    pub(crate) top_hosts: String,
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct StationHost {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) platform: String,
    pub(crate) region: String,
    pub(crate) connected: bool,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) cpu: f32,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) mem: f32,
}

impl Default for StationHost {
    fn default() -> Self {
        Self {
            id: "local".into(),
            name: "local".into(),
            platform: "unknown".into(),
            region: "local".into(),
            connected: true,
            cpu: 0.0,
            mem: 0.0,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct StationAgent {
    pub(crate) id: String,
    pub(crate) host_id: String,
    pub(crate) role: String,
    pub(crate) phase: String,
    pub(crate) status: String,
    pub(crate) task: String,
    pub(crate) provider: String,
    pub(crate) model: String,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) tokens: f32,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) token_cap: f32,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) prompt: f32,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) completion: f32,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) cached: f32,
    pub(crate) cost: f64,
    pub(crate) turns: u32,
    pub(crate) turn_cap: u32,
    pub(crate) autonomy: String,
    pub(crate) worktree: String,
    pub(crate) parent_id: Option<String>,
    pub(crate) needs_approval: bool,
    pub(crate) approval_id: Option<String>,
    pub(crate) approval_command: String,
    pub(crate) approval_category: String,
}

impl Default for StationAgent {
    fn default() -> Self {
        Self {
            id: "agent".into(),
            host_id: "local".into(),
            role: "direct".into(),
            phase: "idle".into(),
            status: "idle".into(),
            task: "idle".into(),
            provider: "unknown".into(),
            model: "unknown".into(),
            tokens: 0.0,
            token_cap: 200_000.0,
            prompt: 0.0,
            completion: 0.0,
            cached: 0.0,
            cost: 0.0,
            turns: 0,
            turn_cap: 0,
            autonomy: "medium".into(),
            worktree: String::new(),
            parent_id: None,
            needs_approval: false,
            approval_id: None,
            approval_command: String::new(),
            approval_category: String::new(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct StationEvent {
    pub(crate) id: String,
    pub(crate) action: String,
    pub(crate) host_id: String,
    pub(crate) session_id: String,
    pub(crate) agent_id: Option<String>,
    pub(crate) ts: String,
    pub(crate) level: String,
    pub(crate) source: String,
    pub(crate) msg: String,
    pub(crate) editable: bool,
    pub(crate) historical: bool,
}

impl Default for StationEvent {
    fn default() -> Self {
        Self {
            id: "event".into(),
            action: String::new(),
            host_id: "local".into(),
            session_id: String::new(),
            agent_id: None,
            ts: String::new(),
            level: "info".into(),
            source: String::new(),
            msg: String::new(),
            editable: false,
            historical: false,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct StationContextSummary {
    pub(crate) available: bool,
    pub(crate) label: String,
    pub(crate) source: String,
    pub(crate) session_id: String,
    pub(crate) session_label: String,
    pub(crate) backend_source: String,
    pub(crate) backend_label: String,
    pub(crate) backend_session_id: String,
    pub(crate) intendant_session_id: String,
    pub(crate) managed_mode: String,
    pub(crate) context_archive: String,
    pub(crate) format: String,
    pub(crate) turn: String,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) tokens: f32,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) effective_window: f32,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) hard_window: f32,
    pub(crate) item_count: u32,
    pub(crate) category_count: u32,
    pub(crate) replay_mode: String,
    pub(crate) replay_count: u32,
    pub(crate) replay_index: u32,
    pub(crate) replay_time: String,
    pub(crate) exact_status: String,
    pub(crate) pressure_state: StationDetailRow,
    pub(crate) top_categories: Vec<StationBreakdown>,
    pub(crate) top_items: Vec<StationDetailRow>,
}

impl Default for StationContextSummary {
    fn default() -> Self {
        Self {
            available: false,
            label: String::new(),
            source: String::new(),
            session_id: String::new(),
            session_label: String::new(),
            backend_source: String::new(),
            backend_label: String::new(),
            backend_session_id: String::new(),
            intendant_session_id: String::new(),
            managed_mode: String::new(),
            context_archive: String::new(),
            format: String::new(),
            turn: String::new(),
            tokens: 0.0,
            effective_window: 0.0,
            hard_window: 0.0,
            item_count: 0,
            category_count: 0,
            replay_mode: "live".into(),
            replay_count: 0,
            replay_index: 0,
            replay_time: String::new(),
            exact_status: "none".into(),
            pressure_state: StationDetailRow::default(),
            top_categories: Vec::new(),
            top_items: Vec::new(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct StationManagedSummary {
    pub(crate) session_id: String,
    pub(crate) session_label: String,
    pub(crate) backend_source: String,
    pub(crate) backend_label: String,
    pub(crate) backend_session_id: String,
    pub(crate) intendant_session_id: String,
    pub(crate) context_archive: String,
    pub(crate) configured_mode: String,
    pub(crate) mode: String,
    pub(crate) status: String,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) used_tokens: f32,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) effective_window: f32,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) hard_window: f32,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) effective_pct: f32,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) hard_pct: f32,
    pub(crate) rewind_only_limit: Option<f32>,
    pub(crate) remaining_to_rewind_only: Option<f32>,
    pub(crate) rewind_only: bool,
    pub(crate) records: u32,
    pub(crate) anchors: u32,
    pub(crate) lineage_groups: u32,
    pub(crate) fission_groups: u32,
    pub(crate) branches: u32,
    pub(crate) error: String,
    pub(crate) action_state: StationManagedActionState,
    pub(crate) activity_signal: StationDetailRow,
    pub(crate) pressure_state: StationDetailRow,
    pub(crate) latest_rewind: StationDetailRow,
    pub(crate) latest_backout: StationDetailRow,
    pub(crate) recent_records: Vec<StationDetailRow>,
    pub(crate) recent_anchors: Vec<StationDetailRow>,
    pub(crate) recent_branches: Vec<StationDetailRow>,
}

impl Default for StationManagedSummary {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            session_label: String::new(),
            backend_source: String::new(),
            backend_label: String::new(),
            backend_session_id: String::new(),
            intendant_session_id: String::new(),
            context_archive: String::new(),
            configured_mode: String::new(),
            mode: "unknown".into(),
            status: "unknown".into(),
            used_tokens: 0.0,
            effective_window: 0.0,
            hard_window: 0.0,
            effective_pct: 0.0,
            hard_pct: 0.0,
            rewind_only_limit: None,
            remaining_to_rewind_only: None,
            rewind_only: false,
            records: 0,
            anchors: 0,
            lineage_groups: 0,
            fission_groups: 0,
            branches: 0,
            error: String::new(),
            action_state: StationManagedActionState::default(),
            activity_signal: StationDetailRow::default(),
            pressure_state: StationDetailRow::default(),
            latest_rewind: StationDetailRow::default(),
            latest_backout: StationDetailRow::default(),
            recent_records: Vec::new(),
            recent_anchors: Vec::new(),
            recent_branches: Vec::new(),
        }
    }
}

#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct StationManagedActionState {
    pub(crate) anchor: String,
    pub(crate) record: String,
    pub(crate) position: String,
    pub(crate) backout_mode: String,
    pub(crate) readiness: String,
    pub(crate) result: String,
    pub(crate) has_reason: bool,
    pub(crate) has_primer: bool,
    pub(crate) can_inspect: bool,
    pub(crate) can_rewind: bool,
    pub(crate) can_backout: bool,
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct StationChangesSummary {
    pub(crate) status: String,
    pub(crate) count: u32,
    pub(crate) added: u32,
    pub(crate) modified: u32,
    pub(crate) deleted: u32,
    pub(crate) external: u32,
    pub(crate) total_added: u32,
    pub(crate) total_removed: u32,
    pub(crate) latest_path: String,
    pub(crate) latest_kind: String,
    pub(crate) recent: Vec<StationDetailRow>,
}

impl Default for StationChangesSummary {
    fn default() -> Self {
        Self {
            status: "clean".into(),
            count: 0,
            added: 0,
            modified: 0,
            deleted: 0,
            external: 0,
            total_added: 0,
            total_removed: 0,
            latest_path: String::new(),
            latest_kind: String::new(),
            recent: Vec::new(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct StationSessionsSummary {
    pub(crate) total: u32,
    pub(crate) active: u32,
    pub(crate) external: u32,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) total_tokens: f32,
    pub(crate) disk_bytes: f64,
    pub(crate) worktrees: u32,
    pub(crate) worktree_dirty: u32,
    pub(crate) worktree_unmerged: u32,
    pub(crate) worktree_active: u32,
    pub(crate) worktree_cleanup: u32,
    pub(crate) worktree_bytes: f64,
    pub(crate) worktree_scan_status: String,
    pub(crate) latest_task: String,
    pub(crate) latest_source: String,
    pub(crate) latest_updated: String,
    pub(crate) index_status: String,
    pub(crate) search_query: String,
    pub(crate) source_filter: String,
    pub(crate) status_filter: String,
    pub(crate) project_filter: String,
    pub(crate) filtered: u32,
    pub(crate) external_targets: Vec<StationDetailRow>,
    pub(crate) filtered_sessions: Vec<StationDetailRow>,
    pub(crate) recent: Vec<StationDetailRow>,
    pub(crate) recent_worktrees: Vec<StationDetailRow>,
}

impl Default for StationSessionsSummary {
    fn default() -> Self {
        Self {
            total: 0,
            active: 0,
            external: 0,
            total_tokens: 0.0,
            disk_bytes: 0.0,
            worktrees: 0,
            worktree_dirty: 0,
            worktree_unmerged: 0,
            worktree_active: 0,
            worktree_cleanup: 0,
            worktree_bytes: 0.0,
            worktree_scan_status: String::new(),
            latest_task: String::new(),
            latest_source: String::new(),
            latest_updated: String::new(),
            index_status: String::new(),
            search_query: String::new(),
            source_filter: String::new(),
            status_filter: String::new(),
            project_filter: String::new(),
            filtered: 0,
            external_targets: Vec::new(),
            filtered_sessions: Vec::new(),
            recent: Vec::new(),
            recent_worktrees: Vec::new(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct StationControlsSummary {
    pub(crate) backend: String,
    pub(crate) command: String,
    pub(crate) sandbox: String,
    pub(crate) approval_policy: String,
    pub(crate) model: String,
    pub(crate) reasoning_effort: String,
    pub(crate) service_tier: String,
    pub(crate) managed_context: String,
    pub(crate) context_archive: String,
    pub(crate) web_search: bool,
    pub(crate) network_access: bool,
    pub(crate) writable_roots: u32,
    pub(crate) new_session_agent: String,
    pub(crate) session_id: String,
    pub(crate) session_label: String,
    pub(crate) session_selection: String,
    pub(crate) session_source: String,
    pub(crate) session_status: String,
    pub(crate) session_command: String,
    pub(crate) session_backend_id: String,
    pub(crate) session_intendant_id: String,
    pub(crate) session_live_id: String,
    pub(crate) session_live_phase: String,
    pub(crate) session_action_id: String,
    pub(crate) session_attach_id: String,
    pub(crate) session_stop_id: String,
    pub(crate) session_managed_context: String,
    pub(crate) session_context_archive: String,
    pub(crate) session_sandbox: String,
    pub(crate) session_approval_policy: String,
    pub(crate) session_config_managed: String,
    pub(crate) session_config_archive: String,
    pub(crate) session_config_result: String,
    pub(crate) session_config_result_kind: String,
    pub(crate) session_config_has_draft: bool,
    pub(crate) session_config_pending: bool,
    pub(crate) session_launch_persistent: bool,
    pub(crate) session_can_config: bool,
    pub(crate) session_can_focus: bool,
    pub(crate) session_can_attach: bool,
    pub(crate) session_can_stop: bool,
    pub(crate) session_can_rename: bool,
    pub(crate) session_can_interrupt: bool,
    pub(crate) session_can_steer: bool,
    pub(crate) session_detached: bool,
    pub(crate) session_active: bool,
    pub(crate) session_is_codex: bool,
    pub(crate) session_service_tier: String,
    pub(crate) session_goal_status: String,
    pub(crate) session_goal_objective: String,
    pub(crate) session_goal_tokens: String,
    pub(crate) external_turn_state: String,
    pub(crate) external_turn_backend: String,
    pub(crate) external_turn_label: String,
    pub(crate) external_turn_detail: String,
    pub(crate) external_turn_session_id: String,
    pub(crate) prompt_mode: String,
    pub(crate) direct_mode: bool,
    pub(crate) draft_chars: u32,
    pub(crate) display_access: String,
    pub(crate) voice_state: String,
    pub(crate) mic_active: bool,
    pub(crate) video_active: bool,
    pub(crate) active_browser: bool,
    pub(crate) browser_workspaces: u32,
    pub(crate) browser_workspace_status: String,
    pub(crate) browser_workspace_detail: String,
    pub(crate) browser_workspace_latest: String,
    pub(crate) browser_workspace_lease: String,
    pub(crate) browser_workspace_id: String,
    pub(crate) browser_workspace_provider: String,
    pub(crate) browser_workspace_url: String,
    pub(crate) browser_workspace_updated: String,
    pub(crate) browser_workspace_can_create: bool,
    pub(crate) browser_workspace_can_acquire: bool,
    pub(crate) browser_workspace_can_close: bool,
    pub(crate) recordings: u32,
    pub(crate) active_recording: String,
    pub(crate) cu_provider: String,
    pub(crate) cu_model: String,
    pub(crate) cu_backend: String,
    pub(crate) cu_validation_state: String,
    pub(crate) cu_validation_detail: String,
    pub(crate) debug_screen: bool,
    pub(crate) debug_recording: bool,
    pub(crate) pending_attachments: u32,
    pub(crate) shared_view_visible: bool,
    pub(crate) shared_view_target: String,
    pub(crate) shared_view_action: String,
    pub(crate) shared_view_note: String,
    pub(crate) shared_view_can_take_input: bool,
    pub(crate) launch_ready: bool,
    pub(crate) launch_missing: String,
    pub(crate) launch_agent: String,
    pub(crate) launch_agent_label: String,
    pub(crate) launch_command: String,
    pub(crate) launch_task_chars: u32,
    pub(crate) launch_project: String,
    pub(crate) launch_mode: String,
    pub(crate) launch_attachments: u32,
    pub(crate) launch_notice: String,
    pub(crate) selected_display_kind: String,
    pub(crate) selected_display_label: String,
    pub(crate) selected_display_target: String,
    pub(crate) selected_display_host_id: String,
    pub(crate) selected_display_id: Option<i32>,
    pub(crate) selected_display_lane_id: String,
    pub(crate) selected_display_status: String,
    pub(crate) selected_display_authority: String,
    pub(crate) selected_display_capture: String,
    pub(crate) selected_display_freshness: String,
    pub(crate) selected_display_telemetry: String,
    pub(crate) selected_display_can_open: bool,
    pub(crate) selected_display_can_focus: bool,
    pub(crate) selected_display_can_take_input: bool,
    pub(crate) selected_display_can_release_input: bool,
    pub(crate) selected_display_can_attach_frame: bool,
    pub(crate) selected_display_can_capture: bool,
    pub(crate) latest_operational_activity: String,
    pub(crate) latest_operational_activity_label: String,
}

impl Default for StationControlsSummary {
    fn default() -> Self {
        Self {
            backend: String::new(),
            command: String::new(),
            sandbox: String::new(),
            approval_policy: String::new(),
            model: String::new(),
            reasoning_effort: String::new(),
            service_tier: String::new(),
            managed_context: String::new(),
            context_archive: String::new(),
            web_search: false,
            network_access: false,
            writable_roots: 0,
            new_session_agent: String::new(),
            session_id: String::new(),
            session_label: String::new(),
            session_selection: String::new(),
            session_source: String::new(),
            session_status: String::new(),
            session_command: String::new(),
            session_backend_id: String::new(),
            session_intendant_id: String::new(),
            session_live_id: String::new(),
            session_live_phase: String::new(),
            session_action_id: String::new(),
            session_attach_id: String::new(),
            session_stop_id: String::new(),
            session_managed_context: String::new(),
            session_context_archive: String::new(),
            session_sandbox: String::new(),
            session_approval_policy: String::new(),
            session_config_managed: String::new(),
            session_config_archive: String::new(),
            session_config_result: String::new(),
            session_config_result_kind: String::new(),
            session_config_has_draft: false,
            session_config_pending: false,
            session_launch_persistent: false,
            session_can_config: false,
            session_can_focus: false,
            session_can_attach: false,
            session_can_stop: false,
            session_can_rename: false,
            session_can_interrupt: false,
            session_can_steer: false,
            session_detached: false,
            session_active: false,
            session_is_codex: false,
            session_service_tier: String::new(),
            session_goal_status: String::new(),
            session_goal_objective: String::new(),
            session_goal_tokens: String::new(),
            external_turn_state: String::new(),
            external_turn_backend: String::new(),
            external_turn_label: String::new(),
            external_turn_detail: String::new(),
            external_turn_session_id: String::new(),
            prompt_mode: String::new(),
            direct_mode: false,
            draft_chars: 0,
            display_access: String::new(),
            voice_state: String::new(),
            mic_active: false,
            video_active: false,
            active_browser: true,
            browser_workspaces: 0,
            browser_workspace_status: String::new(),
            browser_workspace_detail: String::new(),
            browser_workspace_latest: String::new(),
            browser_workspace_lease: String::new(),
            browser_workspace_id: String::new(),
            browser_workspace_provider: String::new(),
            browser_workspace_url: String::new(),
            browser_workspace_updated: String::new(),
            browser_workspace_can_create: false,
            browser_workspace_can_acquire: false,
            browser_workspace_can_close: false,
            recordings: 0,
            active_recording: String::new(),
            cu_provider: String::new(),
            cu_model: String::new(),
            cu_backend: String::new(),
            cu_validation_state: String::new(),
            cu_validation_detail: String::new(),
            debug_screen: false,
            debug_recording: false,
            pending_attachments: 0,
            shared_view_visible: false,
            shared_view_target: String::new(),
            shared_view_action: String::new(),
            shared_view_note: String::new(),
            shared_view_can_take_input: false,
            launch_ready: false,
            launch_missing: String::new(),
            launch_agent: String::new(),
            launch_agent_label: String::new(),
            launch_command: String::new(),
            launch_task_chars: 0,
            launch_project: String::new(),
            launch_mode: String::new(),
            launch_attachments: 0,
            launch_notice: String::new(),
            selected_display_kind: String::new(),
            selected_display_label: String::new(),
            selected_display_target: String::new(),
            selected_display_host_id: String::new(),
            selected_display_id: None,
            selected_display_lane_id: String::new(),
            selected_display_status: String::new(),
            selected_display_authority: String::new(),
            selected_display_capture: String::new(),
            selected_display_freshness: String::new(),
            selected_display_telemetry: String::new(),
            selected_display_can_open: false,
            selected_display_can_focus: false,
            selected_display_can_take_input: false,
            selected_display_can_release_input: false,
            selected_display_can_attach_frame: false,
            selected_display_can_capture: false,
            latest_operational_activity: String::new(),
            latest_operational_activity_label: String::new(),
        }
    }
}

#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct StationAttentionQueueSummary {
    pub(crate) count: u32,
    pub(crate) blocked: u32,
    pub(crate) warn: u32,
    pub(crate) ready: u32,
    pub(crate) items: Vec<StationAttentionItem>,
}

#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct StationAttentionItem {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) level: String,
    pub(crate) title: String,
    pub(crate) meta: String,
    pub(crate) detail: String,
    pub(crate) session_id: String,
    pub(crate) can_cancel: bool,
}

#[derive(Clone, Deserialize, Default)]
#[serde(default)]
pub(crate) struct StationDisplayRunwaySummary {
    pub(crate) selected_peer_id: String,
    pub(crate) selected_peer_label: String,
    pub(crate) selected_display_id: i32,
    pub(crate) selected_peer_connected: bool,
    pub(crate) selected_peer_can_display: bool,
    pub(crate) peer_status: String,
    pub(crate) peer_count: u32,
    pub(crate) connected_peers: u32,
    pub(crate) display_peers: u32,
    pub(crate) operator_session_id: String,
    pub(crate) local_streams: u32,
    pub(crate) remote_streams: u32,
    pub(crate) shared_view_visible: bool,
    pub(crate) lanes: Vec<StationDisplayRunwayLane>,
}

#[derive(Clone, Deserialize, Default)]
#[serde(default)]
pub(crate) struct StationDisplayRunwayLane {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) meta: String,
    pub(crate) detail: String,
    pub(crate) host_id: String,
    pub(crate) display_id: i32,
    pub(crate) session_id: String,
    pub(crate) live_id: String,
    pub(crate) host_label: String,
    pub(crate) lane_label: String,
    pub(crate) resolution: String,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) fps: f32,
    pub(crate) codec: String,
    pub(crate) quality: String,
    pub(crate) telemetry_label: String,
    pub(crate) input_authority: String,
    pub(crate) selected: bool,
    pub(crate) can_focus: bool,
    pub(crate) can_interrupt: bool,
    pub(crate) can_take_input: bool,
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct StationBreakdown {
    pub(crate) category: String,
    pub(crate) label: String,
    #[serde(deserialize_with = "f32_or_default")]
    pub(crate) value: f32,
    pub(crate) count: u32,
    pub(crate) part_id: String,
    pub(crate) detail: String,
}

impl Default for StationBreakdown {
    fn default() -> Self {
        Self {
            category: String::new(),
            label: String::new(),
            value: 0.0,
            count: 0,
            part_id: String::new(),
            detail: String::new(),
        }
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct StationDetailRow {
    pub(crate) id: String,
    pub(crate) session_id: String,
    pub(crate) action: String,
    pub(crate) label: String,
    pub(crate) value: String,
    pub(crate) detail: String,
    pub(crate) tone: String,
    pub(crate) external_status: String,
    pub(crate) backend_id: String,
    pub(crate) intendant_id: String,
    pub(crate) live_id: String,
    pub(crate) action_id: String,
    pub(crate) attach_id: String,
    pub(crate) stop_id: String,
    pub(crate) live_phase: String,
    pub(crate) command: String,
    pub(crate) managed_context: String,
    pub(crate) context_archive: String,
    pub(crate) launch_persistent: bool,
    pub(crate) external_detached: bool,
    pub(crate) is_codex: bool,
    pub(crate) thread_action_session_id: String,
    pub(crate) goal_status: String,
    pub(crate) goal_objective: String,
    pub(crate) goal_tokens: String,
    pub(crate) goal_token_budget: String,
    pub(crate) can_resume: bool,
    pub(crate) can_config: bool,
    pub(crate) can_rename: bool,
    pub(crate) can_focus: bool,
    pub(crate) can_attach: bool,
    pub(crate) can_stop: bool,
    pub(crate) can_interrupt: bool,
    pub(crate) can_restart: bool,
    pub(crate) can_open_log: bool,
    pub(crate) can_fork: bool,
}

pub(crate) fn activity_retained_count(snapshot: &StationSnapshot) -> usize {
    snapshot.activity.retained_count.max(snapshot.events.len())
}

pub(crate) fn f32_or_default<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<f64>::deserialize(deserializer)?.unwrap_or(0.0) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_deserializes_camel_case_with_defaults() {
        let snapshot: StationSnapshot = serde_json::from_value(serde_json::json!({
            "hosts": [{"id": "h1", "name": "Host One", "connected": false, "cpu": null}],
            "agents": [{"id": "a1", "hostId": "h1", "needsApproval": true, "tokens": 12.5}],
            "events": [{"id": "e1", "level": "warn", "msg": "hello"}],
            "controls": {"sessionActive": true, "backend": "codex"},
            "sessions": {"total": 4, "worktreeDirty": 2}
        }))
        .expect("snapshot should deserialize");

        let host = &snapshot.hosts[0];
        assert_eq!(host.id, "h1");
        assert!(!host.connected);
        // f32_or_default maps JSON null to 0.0 instead of failing.
        assert_eq!(host.cpu, 0.0);
        // Unspecified fields take their struct defaults.
        assert_eq!(host.region, "local");

        let agent = &snapshot.agents[0];
        assert_eq!(agent.host_id, "h1");
        assert!(agent.needs_approval);
        assert_eq!(agent.tokens, 12.5);
        assert_eq!(agent.token_cap, 200_000.0);

        assert_eq!(snapshot.events[0].level, "warn");
        assert!(snapshot.controls.session_active);
        assert_eq!(snapshot.controls.backend, "codex");
        assert_eq!(snapshot.sessions.total, 4);
        assert_eq!(snapshot.sessions.worktree_dirty, 2);
        // Sections absent from the payload fall back wholesale.
        assert_eq!(snapshot.context.replay_mode, "live");
    }

    #[test]
    fn display_runway_lane_renames_type_to_kind() {
        let lane: StationDisplayRunwayLane = serde_json::from_value(serde_json::json!({
            "type": "local_stream",
            "id": "lane-1",
            "fps": 30.0
        }))
        .expect("lane should deserialize");
        assert_eq!(lane.kind, "local_stream");
        assert_eq!(lane.fps, 30.0);
    }

    #[test]
    fn activity_retained_count_prefers_the_larger_signal() {
        let mut snapshot = StationSnapshot {
            events: vec![StationEvent::default(), StationEvent::default()],
            ..Default::default()
        };
        assert_eq!(activity_retained_count(&snapshot), 2);
        snapshot.activity.retained_count = 40;
        assert_eq!(activity_retained_count(&snapshot), 40);
    }
}
