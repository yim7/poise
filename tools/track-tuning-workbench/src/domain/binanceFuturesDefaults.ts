import type { TrackDraft, TrackDraftAttachments } from '@/domain/trackDraft';

export const BINANCE_FUTURES_DEFAULT_EXCHANGE_RULES = Object.freeze({
  makerFeeRate: 0.0002,
  takerFeeRate: 0.0005,
});

export function withBinanceFuturesDefaults(draft: TrackDraft): TrackDraft {
  return {
    ...draft,
    attachments: mergeBinanceFuturesDefaults(draft.attachments),
  };
}

function mergeBinanceFuturesDefaults(
  attachments: TrackDraftAttachments,
): TrackDraftAttachments {
  return {
    ...attachments,
    exchangeRules: {
      ...BINANCE_FUTURES_DEFAULT_EXCHANGE_RULES,
      ...attachments.exchangeRules,
    },
  };
}
