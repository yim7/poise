export interface InstrumentView { venue: string; symbol: string }
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
    out_of_band_policy: string | Record<string, unknown>
  }
  max_notional: number
  market: { mark_price: number | null; best_bid: number | null; best_ask: number | null }
  position: { current_exposure: number; quantity_unit: string; quantity: number; notional: number | null; notional_asset: string }
  position_to_execution_gap: number | null
  decision: InventoryDecisionView
  pnl: TrackPnlView
  execution: TrackExecutionView
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
