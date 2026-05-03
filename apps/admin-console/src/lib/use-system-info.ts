import { useEffect, useState } from "react";
import { configApi, type SystemInfo } from "./config-api";

let cached: SystemInfo | null | undefined;
const subscribers = new Set<(value: SystemInfo | null) => void>();
let inflight: Promise<SystemInfo | null> | null = null;

function fetchOnce(): Promise<SystemInfo | null> {
  if (cached !== undefined) return Promise.resolve(cached);
  if (inflight) return inflight;
  inflight = configApi
    .systemInfo()
    .then((info) => {
      cached = info;
      subscribers.forEach((cb) => cb(info));
      return info;
    })
    .catch(() => {
      cached = null;
      subscribers.forEach((cb) => cb(null));
      return null;
    })
    .finally(() => {
      inflight = null;
    });
  return inflight;
}

/** Module-cached system info. First caller fetches, others reuse the result. */
export function useSystemInfo(): SystemInfo | null {
  const [info, setInfo] = useState<SystemInfo | null>(cached ?? null);
  useEffect(() => {
    let active = true;
    if (cached !== undefined) {
      setInfo(cached);
    } else {
      void fetchOnce().then((value) => {
        if (active) setInfo(value);
      });
    }
    const cb = (value: SystemInfo | null) => {
      if (active) setInfo(value);
    };
    subscribers.add(cb);
    return () => {
      active = false;
      subscribers.delete(cb);
    };
  }, []);
  return info;
}
