export interface InstrumentView { venue: string; symbol: string }
export type TrackPositionQuantityUnitView = 'base_asset' | 'contracts'
export type ShapeFamily = 'linear' | 'inertial' | 'responsive'
export type BandFlattenTrigger = 'immediate' | { flatten_confirm: { bps: number } }
export type BandRecoverPolicy = 'back_in_band' | { reentry_confirm: { bps: number } }
export type BandProtectionPolicy = 'freeze' | 'terminate' | { flatten: { trigger: BandFlattenTrigger; recover: BandRecoverPolicy } }
export interface TrackActivityItemView { ts: string; message: string; level: 'info' | 'warn' | 'error' }
export interface TrackLifecycleView { status: string; updated_at: string }
export interface ReadinessIssueView { component: string; reason: string }
export interface TradingReadinessResponse { ready_to_trade: boolean; blockers: ReadinessIssueView[]; warnings: ReadinessIssueView[] }

export interface AccountSummaryView {
  valuation_asset: string | null
  equity: number | null
  available: number | null
  unrealized_pnl: number | null
  risk_signal: string
  updated_at: string | null
}

export interface InventoryDecisionView {
  revision: number | null
  policy_target: number | null
  risk_approved_target: number | null
  acquisition_frontier: number | null
  execution_target: number | null
  reason: string
}

export interface TrackPnlView {
  pnl_asset: string
  total_pnl: number | null
  completeness: string
  unvalued_cashflows: unknown[]
}

export interface ExecutionBindingView {
  id: string
  label: string
  status: string
  intent: string
  order?: { side: string; price: number; quantity: number }
}

export interface AcquisitionSessionView {
  acquisition_reset_reason: string
  acquisition_session_started_at: string
  initial_frontier: number | null
  initial_release_units: number | null
}

export interface RiskAcquisitionView {
  direction: string
  backlog_units: number
  release_anchor_price: number
  release_anchor_target: number
  stale_release_elapsed_minutes: number
  stale_release_minutes: number
  next_advantage_target: number
  next_advantage_price: number | null
  next_release_units: number
  next_release_target: number
}

export interface TrackExecutionView {
  state: string
  execution_status: string
  attention_reasons: string[]
  price_execution_block_reason?: string | null
  active_binding_count: number
  acquisition_session?: AcquisitionSessionView | null
  risk_acquisition?: RiskAcquisitionView | null
  bindings: ExecutionBindingView[]
}

export interface ExecutionBadgeView {
  state: string
  execution_status: string
  attention_reasons: string[]
  price_execution_block_reason?: string | null
  active_binding_count: number
}

export interface TrackListItemView {
  id: string
  instrument: InstrumentView
  lifecycle: TrackLifecycleView
  strategy_price: number | null
  strategy_price_status: string
  exposure: { current: number }
  decision: InventoryDecisionView
  execution: ExecutionBadgeView
  pnl: TrackPnlView
}

export interface TrackListResponse { items: TrackListItemView[] }

export interface TrackDetailView {
  identity: { id: string; instrument: InstrumentView }
  status: { lifecycle: TrackLifecycleView; strategy_price: number | null; strategy_price_status: string }
  strategy: {
    lower_price: number
    upper_price: number
    long_exposure_units: number
    short_exposure_units: number
    notional_per_unit: number
    out_of_band_policy: BandProtectionPolicy
  }
  max_notional: number
  market: { mark_price: number | null; best_bid: number | null; best_ask: number | null }
  position: { current_exposure: number; quantity_unit: TrackPositionQuantityUnitView; quantity: number; notional: number | null; notional_asset: string }
  position_to_execution_gap: number | null
  decision: InventoryDecisionView
  pnl: TrackPnlView
  execution: TrackExecutionView
  activity: TrackActivityItemView[]
}

export interface RiskAcquisitionConfigurationView {
  initial_ratio: number
  advantage_steps: number
  min_release_steps: number
  max_release_steps: number
  catchup_ratio: number
  stale_release_minutes: number
}

export interface TrackConfigurationActiveView {
  lower_price: number
  upper_price: number
  long_exposure_units: number
  short_exposure_units: number
  notional_per_unit: number
  min_rebalance_units: number
  shape_family: ShapeFamily
  out_of_band_policy: BandProtectionPolicy
  max_notional: number
  daily_loss_limit: number
  total_loss_limit: number
  risk_acquisition: RiskAcquisitionConfigurationView
}

export interface TrackConfigurationResponse {
  version: string
  identity: { id: string; instrument: InstrumentView }
  active: TrackConfigurationActiveView
  restart_required: { leverage: number; tick_timeout_secs: number }
  application_status: TrackConfigurationApplicationStatus
  application: TrackConfigurationApplicationView | null
}

export type TrackConfigurationApplicationStatus = 'idle' | 'preparing' | 'waiting_for_orders' | 'activating' | 'active' | 'failed'

export interface TrackConfigurationApplicationView {
  application_id: string
  status: TrackConfigurationApplicationStatus
  accepted: boolean
  message: string | null
  active_version: string | null
}

export interface TrackConfigurationImpactView {
  strategy_price: number
  position_quantity: number
  position_quantity_unit: TrackPositionQuantityUnitView
  current_exposure: number
  candidate_current_exposure: number
  active_policy_target: number | null
  candidate_curve_target: number
  candidate_policy_target: number
  candidate_policy_state: 'in_band' | 'frozen' | 'flattening' | 'flatten_pending' | 'terminated'
  candidate_risk_approved_target: number
  candidate_risk_outcome: 'allowed' | 'capped' | 'daily_loss_terminated' | 'total_loss_terminated'
  active_policy_target_position_qty: number | null
  candidate_policy_target_position_qty: number
  target_position_qty_delta: number | null
  active_policy_target_signed_notional: number | null
  candidate_policy_target_signed_notional: number
  target_signed_notional_delta: number | null
  candidate_target_delta_from_current_exposure: number
  candidate_position_qty_delta_from_current: number
  initial_frontier: number
  initial_execution_target: number
  position_qty_per_unit: number
  minimum_rebalance_position_qty: number
  normalized_minimum_rebalance_position_qty: number
  minimum_rebalance_notional: number
  maximum_target_position_qty: number
  notional_asset: string
}

export interface TrackConfigurationCapacityProofView {
  available_increase_notional: number
  required_increase_notional: number
  asset: string
  observed_at: string
}

export interface TrackConfigurationPreviewResponse {
  previewed_at: string
  valid: boolean
  field_errors: { field: string; message: string }[]
  blockers: { code: string; message: string }[]
  impact: TrackConfigurationImpactView | null
  capacity_proof: TrackConfigurationCapacityProofView | null
}

export interface TrackLiveView {
  strategy_price: number | null
  strategy_price_status: string
  mark_price: number | null
  best_bid: number | null
  best_ask: number | null
  position_to_execution_gap: number | null
  decision: InventoryDecisionView
  acquisition_session?: AcquisitionSessionView | null
  risk_acquisition?: RiskAcquisitionView | null
  price_execution_block_reason?: string | null
}

export type StreamEvent =
  | { type: 'track_list_item_changed'; track_id: string; item: TrackListItemView }
  | { type: 'track_detail_changed'; track_id: string; detail: TrackDetailView }
  | { type: 'track_live_view_changed'; track_id: string; live: TrackLiveView }
  | { type: 'account_summary_changed'; summary: AccountSummaryView }
