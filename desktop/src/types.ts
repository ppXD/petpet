export interface UsageEvent {
  id: string;
  provider: "claude_code" | "codex" | "custom_api";
  session_id: string;
  project_path: string | null;
  git_branch: string | null;
  model: string;
  timestamp: string;
  tokens: {
    input: number;
    output: number;
    cache_read: number;
    cache_creation: number;
    reasoning: number;
  };
  kind: { type: string; [k: string]: unknown };
  source: { file: string; byte_offset: number; line: number };
}

/**
 * Emitted by backend on every XP delta (pet://state) and additionally
 * on level boundary crossings (pet://level_up). Mirrors `PetStateUpdate`
 * in `src/xp/engine.rs`. The frontend keeps the latest one in a single
 * piece of state and re-renders the pet + badge from it.
 */
export interface PetStateUpdate {
  pet_id: string;
  species_id: string;
  name: string;
  name_finalized: boolean;

  total_xp: number;
  current_level: number;
  xp_in_level: number;
  xp_for_next_level: number | null;

  stage_level: number;
  stage_name: string | null;
  sprite_key: string | null;
  stage_flavor: string | null;

  next_evolution_level: number | null;
  next_evolution_name: string | null;
  xp_to_next_evolution: number | null;

  leveled_up: boolean;
  level_before: number;
  level_after: number;
  evolved: boolean;
  stage_level_before: number;
  stage_level_after: number;
  level_up_flavor: string | null;
  stage_metadata: {
    events?: Record<string, CeremonyAction[]>;
    attributes?: Record<string, unknown>;
  } | null;
}

/**
 * One step of a ceremony. Templates author these as JSON arrays in
 * `stages/stage_N/on_enter.json` etc. The CeremonyPlayer interprets
 * them by `kind`, scheduling each one at `after` for duration `for`.
 */
export interface CeremonyAction {
  kind: CeremonyKind;
  after?: string;       // delay before this step starts, e.g. "1s", "500ms"
  for?: string;         // how long the effect runs
  intensity?: number;   // 1-10 for shake
  count?: number;       // particles / confetti
  color?: string;
  colors?: string[];
  text?: string;        // bubble / modal body — supports {{pet.name}} {{stage.name}}
  title?: string;
  body?: string;
  calls?: string;       // Tauri command on modal confirm
  confirm_label?: string;
  cancel_label?: string;
  [k: string]: unknown;
}

export type CeremonyKind =
  | "shake"
  | "crack"
  | "flash"
  | "burst"
  | "ring"
  | "sparkle"
  | "bubble"
  | "confetti"
  | "modal";
