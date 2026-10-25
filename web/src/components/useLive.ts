import { useEffect, useRef } from 'react';

/** Subscribe to `/api/events` (incident changes); calls `onEvent` at most every `throttleMs`. */
export function useIncidentEvents(onEvent: (id: string) => void, throttleMs = 800) {
  const cb = useRef(onEvent);
  cb.current = onEvent;
  useEffect(() => {
    const es = new EventSource('/api/events');
    let timer: number | undefined;
    let last = '';
    es.onmessage = (msg) => {
      try {
        const data = JSON.parse(msg.data) as { type: string; id?: string };
        if (data.type !== 'incident' || !data.id) return;
        last = data.id;
        if (timer) return;
        timer = window.setTimeout(() => {
          timer = undefined;
          cb.current(last);
        }, throttleMs);
      } catch {
        /* ignore malformed frames */
      }
    };
    return () => {
      es.close();
      if (timer) window.clearTimeout(timer);
    };
  }, [throttleMs]);
}
