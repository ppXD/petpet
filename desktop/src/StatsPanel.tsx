import type { UsageEvent } from "./types";

export interface StatsRow {
  provider: string;
  model: string;
  events: number;
  input: number;
  output: number;
  cache_read: number;
  cache_creation: number;
  reasoning: number;
}

interface ActivityEvent {
  id: string;
  provider: string;
  kind: { type: string; [k: string]: unknown };
}

interface Props {
  rows: StatsRow[];
  lastEvent: UsageEvent | null;
  lastActivity?: ActivityEvent | null;
  onClose: () => void;
}

export function StatsPanel({ rows, lastEvent, lastActivity, onClose }: Props) {
  const grouped = new Map<string, StatsRow[]>();
  for (const r of rows) {
    const arr = grouped.get(r.provider) ?? [];
    arr.push(r);
    grouped.set(r.provider, arr);
  }

  return (
    <div className="panel">
      <div className="panel-head">
        <span>Usage</span>
        <button className="panel-close" onClick={onClose} aria-label="close">×</button>
      </div>

      {lastEvent && (
        <div className="panel-recent">
          <span className={`tag tag-${lastEvent.provider}`}>{providerLabel(lastEvent.provider)}</span>
          <span className="model">{lastEvent.model}</span>
          <span className="delta">
            +{lastEvent.tokens.output.toLocaleString()} out
          </span>
        </div>
      )}

      {lastActivity && (
        <div className="panel-activity">
          <span className={`tag tag-${lastActivity.provider}`}>{providerLabel(lastActivity.provider)}</span>
          <span className="activity-kind">{lastActivity.kind.type.replace(/_/g, " ")}</span>
        </div>
      )}

      <div className="panel-body">
        {[...grouped.entries()].map(([provider, items]) => (
          <div key={provider} className="provider-block">
            <div className="provider-head">{providerLabel(provider)}</div>
            {items.map((r) => (
              <div key={r.provider + r.model} className="model-row">
                <span className="model-name" title={r.model}>{shortModel(r.model)}</span>
                <span className="model-stats">
                  <span title="output tokens">{formatNum(r.output)}</span>
                  <span className="dim">/</span>
                  <span title="events" className="dim">{formatNum(r.events)}</span>
                </span>
              </div>
            ))}
          </div>
        ))}
        {rows.length === 0 && <div className="panel-empty">No events yet.</div>}
      </div>
    </div>
  );
}

function providerLabel(p: string): string {
  switch (p) {
    case "claude_code": return "Claude Code";
    case "codex": return "Codex";
    case "custom_api": return "Custom API";
    default: return p;
  }
}

function shortModel(m: string): string {
  return m
    .replace(/^claude-/, "")
    .replace(/-\d{8}$/, "")
    .replace(/^gpt-/, "");
}

function formatNum(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + "M";
  if (n >= 1_000) return (n / 1_000).toFixed(1) + "k";
  return String(n);
}
