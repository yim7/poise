import { useEffect, useMemo, useRef, useState } from 'react';

import type {
  WorkbenchBridge,
  WorkbenchEvaluationReport,
  WorkbenchRulesFixtureId,
} from '@/app/workbenchBridge';
import type { TrackDraft } from '@/domain/trackDraft';
import { validateTrackDraft } from '@/domain/trackValidation';

export type TrackEvaluationStatus =
  | 'unavailable'
  | 'idle'
  | 'blocked'
  | 'loading'
  | 'ready'
  | 'error';

export interface TrackEvaluationState {
  status: TrackEvaluationStatus;
  report: WorkbenchEvaluationReport | null;
  message: string | null;
}

export interface UseTrackEvaluationOptions {
  bridge?: WorkbenchBridge;
  draft: TrackDraft | null;
  exchangeVenue: string;
  rulesFixture: WorkbenchRulesFixtureId | null;
}

const BROWSER_UNAVAILABLE_MESSAGE =
  '当前是浏览器预览，Rust evaluator 不可用；不会运行 TypeScript 计算回退。';

export function useTrackEvaluation({
  bridge,
  draft,
  exchangeVenue,
  rulesFixture,
}: UseTrackEvaluationOptions): TrackEvaluationState {
  const generation = useRef(0);
  const [state, setState] = useState<TrackEvaluationState>(() => ({
    status: bridge?.isTauriEnvironment() ? 'idle' : 'unavailable',
    report: null,
    message: bridge?.isTauriEnvironment() ? null : BROWSER_UNAVAILABLE_MESSAGE,
  }));
  const requestKey = useMemo(() => strategyRequestKey(draft), [draft]);
  const blockingMessage = useMemo(() => strategyBlockingMessage(draft), [requestKey]);

  useEffect(() => {
    const requestGeneration = ++generation.current;

    if (!bridge?.isTauriEnvironment()) {
      setState({
        status: 'unavailable',
        report: null,
        message: BROWSER_UNAVAILABLE_MESSAGE,
      });
      return;
    }

    if (!draft) {
      setState({ status: 'idle', report: null, message: '请先选择 Track。' });
      return;
    }

    if (!rulesFixture) {
      setState({
        status: 'blocked',
        report: null,
        message: '请选择完整的评估规则 fixture。',
      });
      return;
    }

    if (blockingMessage) {
      setState({ status: 'blocked', report: null, message: blockingMessage });
      return;
    }

    let cancelled = false;
    setState({ status: 'loading', report: null, message: null });

    void bridge.evaluateTrack({
      draft,
      exchangeVenue,
      rulesFixture,
    }).then(
      (report) => {
        if (cancelled || generation.current !== requestGeneration) {
          return;
        }
        setState({ status: 'ready', report, message: null });
      },
      (error: unknown) => {
        if (cancelled || generation.current !== requestGeneration) {
          return;
        }
        setState({
          status: 'error',
          report: null,
          message: error instanceof Error ? error.message : String(error),
        });
      },
    );

    return () => {
      cancelled = true;
    };
  }, [blockingMessage, bridge, draft?.draftId, exchangeVenue, requestKey, rulesFixture]);

  return state;
}

function strategyRequestKey(draft: TrackDraft | null) {
  if (!draft) {
    return '';
  }
  return JSON.stringify({
    draftId: draft.draftId,
    additional: draft.additional,
    rawNumbers: draft.rawNumbers,
    riskAcquisition: draft.riskAcquisition,
    enums: draft.enums,
    loadIssues: draft.attachments.loadIssues,
  });
}

function strategyBlockingMessage(draft: TrackDraft | null) {
  if (!draft) {
    return null;
  }
  const issues = validateTrackDraft(draft).issues.filter(
    (issue) => issue.field !== 'quotePriceInput',
  );
  if (issues.length === 0) {
    return null;
  }
  return `策略配置仍有错误：${issues.map((issue) => issue.message).join('；')}`;
}
