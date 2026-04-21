import {
  DEFAULT_BINANCE_FUTURES_EXCHANGE_RULES as BINANCE_FUTURES_DEFAULT_EXCHANGE_RULES,
  type TrackDraft,
  withDefaultBinanceFuturesExchangeRules,
} from '@/domain/trackDraft';

export { BINANCE_FUTURES_DEFAULT_EXCHANGE_RULES };

export function withBinanceFuturesDefaults(draft: TrackDraft): TrackDraft {
  return {
    ...draft,
    attachments: withDefaultBinanceFuturesExchangeRules(draft.attachments),
  };
}
