## ADDED Requirements

### Requirement: ask_user tool schema
The system SHALL expose a tool named `ask_user` with parameters: `question` (required string), `context` (optional string), `options` (optional array of `{label, description?, recommended?, is_custom?}`), `allow_custom` (default true), `require_confirm` (default true), `timeout_secs` (5..600, default 15), `pause_on_activity` (default true), `resume_after_idle_secs` (default 5). The total tool count SHALL be 25.

#### Scenario: Schema is exposed with default values
- **WHEN** LLM receives the tool schema list
- **THEN** `ask_user` is present with all default values matching the spec above

#### Scenario: Tool count assertion
- **WHEN** `cargo test` runs the schema-count test
- **THEN** the assertion expects 25 tools (24 prior + ask_user)

### Requirement: ask_user tool validation
The system SHALL reject calls where `options` contains more than one item with `recommended: true`. The system SHALL accept zero or one recommended option.

#### Scenario: Multiple recommended options are rejected
- **WHEN** agent calls `ask_user` with 2+ options where `recommended=true`
- **THEN** tool returns an error explaining at most 1 recommended option is allowed
- **AND** no AwaitUserRequest is emitted to the frontend

#### Scenario: Zero or one recommended option is accepted
- **WHEN** agent calls `ask_user` with 0 or 1 `recommended=true` option
- **THEN** tool proceeds normally

### Requirement: AwaitUserRequest lifecycle
The driver SHALL emit a `SessionEvent::AwaitUserRequest` (carrying `question_id`, `question`, options, timing config) to the frontend on tool invocation. The driver SHALL freeze the current turn until ONE of: (a) `AwaitUserAnswer` with `question_id` match arrives, (b) `timeout_secs` elapses, (c) external `UserMessage` or `Reset` interrupts.

#### Scenario: Frontend receives AwaitUserRequest
- **WHEN** `tool_ask_user` is invoked
- **THEN** driver emits `AwaitUserRequest` via `app.emit("await-user", payload)` with a UUID `question_id`

#### Scenario: Only one ask_user can be active at a time
- **WHEN** an `ask_user` is already awaiting and agent calls another tool
- **THEN** the new tool returns an error explaining the conflict
- **AND** the existing await continues

### Requirement: Four terminal states
The tool SHALL terminate in exactly one of four states, all written as a single `tool_result` message: (1) `answered: true, answer: <user input>, auto_submitted: false` — user confirmed; (2) `auto_submitted: true, answer: <recommended option label or empty>` — timeout fired; (3) `aborted: true, answered: false` — user clicked skip; (4) `cancelled: true, answered: false` — global interrupt or external UserMessage interrupted.

#### Scenario: User confirmed within timeout
- **WHEN** user clicks "确认提交" with a selected option or custom input
- **THEN** tool_result contains `{answered: true, answer: <their text>}` and turn resumes

#### Scenario: Timeout with one recommended option
- **WHEN** timeout_secs elapses with exactly one `recommended=true` option
- **THEN** tool_result contains `{answered: true, auto_submitted: true, answer: <recommended label>}` and turn resumes

#### Scenario: Timeout with no recommended option falls back to continue
- **WHEN** timeout_secs elapses with zero `recommended=true` options
- **THEN** tool_result contains `{answered: false, fallback: "no_recommended", note: <chinese>}`
- **AND** agent must decide based on existing context

#### Scenario: User skipped
- **WHEN** user clicks "跳过" button
- **THEN** tool_result contains `{aborted: true, answered: false}` and turn resumes

#### Scenario: Interrupted by global interrupt or external UserMessage
- **WHEN** user clicks global interrupt button OR sends a regular message in the chat
- **THEN** tool_result contains `{cancelled: true, answered: false}` and the external UserMessage enters history normally

### Requirement: Frontend three-step UI
The frontend SHALL render a modal panel with: (1) question text, (2) option chips where `recommended: true` options are visually highlighted (e.g., ✦ ★ marker), (3) "自定义" option (if `allow_custom: true`) that switches textarea to input mode, (4) preview pane showing current selection, (5) "确认提交" button (disabled until selection made) and "跳过" button, (6) countdown timer displaying remaining seconds.

#### Scenario: Three-step flow with option
- **WHEN** user hovers an option
- **THEN** countdown timer pauses (if `pause_on_activity: true`)
- **AND** when timer pauses, "动键盘暂停" hint appears

#### Scenario: Custom input mode
- **WHEN** user clicks the option with `is_custom: true`
- **THEN** textarea switches to editable input mode
- **AND** user types text, countdown pauses per character/keystroke

#### Scenario: Confirm button requires selection first
- **WHEN** modal opens with no selection yet
- **THEN** "确认提交" button is disabled
- **AND** after selecting an option or entering custom text, button becomes enabled

### Requirement: Countdown pause and resume
The countdown SHALL default to 15 seconds. If `pause_on_activity: true`, the timer SHALL pause on any keyboard/mouse activity (hover option, focus input, click) and resume `resume_after_idle_secs` (default 5) seconds after last activity.

#### Scenario: Default countdown behavior
- **WHEN** modal opens with `timeout_secs=15`
- **THEN** countdown starts at 15 and decrements every second
- **AND** on hover option, timer pauses
- **AND** after 5s of no activity, timer resumes from where it paused

### Requirement: History recording
User answers SHALL be recorded in `history/{date}.jsonl` as `kind=tool_result` events paired with the original `ask_user` tool_call via `call_id` (carried on AskUserHandle, backfilled by run_turn). No separate user bubble is created. Dream trigger SHALL NOT count these toward the user-turn cap (they are tool results per P1 rule 4).

#### Scenario: Answer recorded as paired tool result
- **WHEN** user confirms an answer (or timeout/skip/cancel resolves the ask)
- **THEN** a `kind=tool_result` event with `name="ask_user"` and the original call_id is appended to today's history jsonl
- **AND** message rebuild pairs it with the assistant tool_call — no orphan, no strip needed

#### Scenario: Terminal state prefixes distinguish answer provenance
- **WHEN** the answer is auto-submitted (timeout with recommended option)
- **THEN** the tool_result content is `[用户超时未答,已自动提交推荐项] <label>`
- **WHEN** user answers manually
- **THEN** the tool_result content is the raw answer text

#### Scenario: Live tool-result event for UI pairing
- **WHEN** the ask resolves (answer/timeout/skip/interrupt)
- **THEN** the driver emits `llm-tool-result` with name="ask_user" so the frontend pairs it with the earlier tool_call card
