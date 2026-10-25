'use client';

// Drop into client-ui-next (e.g. components/McQueenInit.tsx) and render once in
// the root layout: <McQueenInit />. Copy mcqueen.ts next to it.
import { useEffect } from 'react';
import { installMcQueen } from './mcqueen';

export function McQueenInit() {
  useEffect(() => {
    const ingest = process.env.NEXT_PUBLIC_MCQUEEN_URL; // e.g. https://triage.regere.ai
    if (!ingest) return;
    installMcQueen({
      ingestUrl: `${ingest}/api/ingest/frontend`,
      ingestKey: process.env.NEXT_PUBLIC_MCQUEEN_KEY,
      triageUrl: ingest,
      // Same-origin /api, /proxy calls plus launchpad if it is on another host.
      propagateTo: [/^\/(api|proxy)\//, process.env.NEXT_PUBLIC_LAUNCHPAD_API_URL ?? ''].filter(Boolean),
      // 403/409/422 are worth explaining to frontend devs too.
      alsoReport: [403, 409, 422],
      appVersion: process.env.NEXT_PUBLIC_APP_VERSION,
    });
  }, []);
  return null;
}
