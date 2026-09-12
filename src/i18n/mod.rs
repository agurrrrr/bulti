//! 다국어(i18n) 지원 — 영어(기본)·한국어·일본어.
//!
//! 메시지 키는 **영어 원문**을 그대로 쓴다. 키에 대한 번역이 없으면 영어가
//! 그대로 출력되므로, 번역을 빠뜨려도 UI 가 깨지지 않는다.
//!
//! - [`tr`]: 정적 문자열 조회 (`&'static str`).
//! - [`tr_fmt`]: `{}` 자리표시자를 순서대로 치환한 문자열 생성.
//! - [`set_language`] / [`current`]: 전역 현재 언어. `main` 이 설정 파일에서
//!   읽어 한 번 설정하고, `/language` 커맨드가 실행 중에 바꾼다.
//!
//! 문자열이 많지 않고 UI 경로에서만 조회되므로 선형 탐색으로 충분하다.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU8, Ordering};

/// 지원 언어.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    /// 영어 (기본).
    #[default]
    En,
    /// 한국어.
    Ko,
    /// 일본어.
    Ja,
}

impl Language {
    /// 지원 언어 전체 (표시 순서).
    pub const ALL: [Language; 3] = [Language::En, Language::Ko, Language::Ja];

    /// ISO 639-1 코드 (`en`·`ko`·`ja`).
    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Ko => "ko",
            Self::Ja => "ja",
        }
    }

    /// 코드 문자열을 언어로 변환한다. 대소문자 무시, 미지원이면 `None`.
    pub fn from_code(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "en" | "english" => Some(Self::En),
            "ko" | "kr" | "korean" => Some(Self::Ko),
            "ja" | "jp" | "japanese" => Some(Self::Ja),
            _ => None,
        }
    }

    /// 해당 언어의 자기 이름 (언어 선택 UI 용).
    pub fn native_name(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::Ko => "한국어",
            Self::Ja => "日本語",
        }
    }

    /// 시스템 프롬프트에 넣을 응답 언어 지시문.
    pub fn response_instruction(self) -> &'static str {
        match self {
            Self::En => "Respond to the user in English.",
            Self::Ko => "Respond to the user in Korean (한국어).",
            Self::Ja => "Respond to the user in Japanese (日本語).",
        }
    }
}

/// 전역 현재 언어 인덱스. 0=en, 1=ko, 2=ja.
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// 현재 언어를 반환한다.
pub fn current() -> Language {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Language::Ko,
        2 => Language::Ja,
        _ => Language::En,
    }
}

/// 현재 언어를 설정한다.
pub fn set_language(lang: Language) {
    let idx = match lang {
        Language::En => 0,
        Language::Ko => 1,
        Language::Ja => 2,
    };
    CURRENT.store(idx, Ordering::Relaxed);
}

/// 번역 카탈로그: (영어 원문, 한국어, 일본어).
///
/// 영어는 키 자체이므로 별도 항목이 필요 없다. 조회 실패 시 키(영어)를 반환한다.
const CATALOG: &[(&str, &str, &str)] = &[
    // ── 슬래시 커맨드 설명 ─────────────────────────────
    ("Exit conversation", "대화 종료", "会話を終了"),
    (
        "Start a new session",
        "새 세션 시작",
        "新しいセッションを開始",
    ),
    ("Command help", "명령 도움말", "コマンドのヘルプ"),
    ("Resume a session", "세션 재개", "セッションを再開"),
    ("Switch model", "모델 전환", "モデルの切り替え"),
    (
        "Set reasoning effort",
        "reasoning effort 설정",
        "reasoning effort を設定",
    ),
    (
        "Show endpoint config (active / all / named)",
        "엔드포인트 설정 조회 (활성·전체·특정 이름)",
        "エンドポイント設定の表示 (有効・全件・指定名)",
    ),
    (
        "Show MCP servers (list / named detail)",
        "MCP 서버 조회 (목록·특정 이름 상세)",
        "MCP サーバーの表示 (一覧・指定名の詳細)",
    ),
    (
        "Show current session info (id / model / context usage)",
        "현재 세션 정보 표시 (id·모델·컨텍스트 사용량)",
        "現在のセッション情報を表示 (id・モデル・コンテキスト使用量)",
    ),
    ("List sessions", "세션 목록 조회", "セッション一覧"),
    (
        "Compact conversation history (summarize to shrink context)",
        "대화 히스토리 컴팩트 (요약으로 컨텍스트 축소)",
        "会話履歴を圧縮 (要約してコンテキストを削減)",
    ),
    (
        "Fork the current session (copy to a new session id)",
        "현재 세션을 분기 (새 세션 id 로 복제)",
        "現在のセッションを分岐 (新しいセッション id に複製)",
    ),
    (
        "Export conversation history to a markdown file",
        "대화 기록 마크다운 파일로 내보내기",
        "会話履歴を Markdown ファイルにエクスポート",
    ),
    (
        "Show session token and cost usage",
        "세션 토큰·비용 사용량 표시",
        "セッションのトークン・コスト使用量を表示",
    ),
    (
        "Browse prompt history (↑/↓ keys or this command)",
        "프롬프트 히스토리 탐색 (↑/↓ 키 또는 이 커맨드로)",
        "プロンプト履歴を閲覧 (↑/↓ キーまたはこのコマンド)",
    ),
    (
        "Change language (en / ko / ja)",
        "언어 변경 (en·ko·ja)",
        "言語を変更 (en・ko・ja)",
    ),
    // 슬래시 인자 자동완성 라벨
    ("model", "모델", "モデル"),
    ("endpoint", "엔드포인트", "エンドポイント"),
    ("MCP server", "MCP 서버", "MCP サーバー"),
    ("language", "언어", "言語"),
    // ── 공용 ───────────────────────────────────────────
    ("Yes", "예", "はい"),
    ("No", "아니요", "いいえ"),
    ("files", "파일", "ファイル"),
    ("none", "없음", "なし"),
    // ── TUI ────────────────────────────────────────────
    (
        "Bulti Chat — {endpoint} / {model}",
        "불티(Bulti) 대화형 채팅 — {endpoint} / {model}",
        "Bulti チャット — {endpoint} / {model}",
    ),
    (
        "Input (Enter send · Shift+Enter newline · ↑↓ history · Tab complete · Ctrl+T thinking · Ctrl+Q quit)",
        "입력 (Enter 전송 · Shift+Enter 줄바꿈 · ↑↓ 히스토리 · Tab 자동완성 · Ctrl+T 생각 · Ctrl+Q 종료)",
        "入力 (Enter 送信 · Shift+Enter 改行 · ↑↓ 履歴 · Tab 補完 · Ctrl+T 思考 · Ctrl+Q 終了)",
    ),
    (
        "Command completion {}/{} (Enter select · Tab/↑↓ move · Esc cancel)",
        "커맨드 자동완성 {}/{} (Enter 선택 · Tab/↑↓ 이동 · Esc 취소)",
        "コマンド補完 {}/{} (Enter 選択 · Tab/↑↓ 移動 · Esc 取消)",
    ),
    (
        "Session saved — {id}",
        "세션 저장됨 — {id}",
        "セッション保存済み — {id}",
    ),
    ("Session id: {id}", "세션 id: {id}", "セッション id: {id}"),
    (
        "  Session ↑{} ↓{}",
        "  세션 ↑{} ↓{}",
        "  セッション ↑{} ↓{}",
    ),
    (
        "Session is auto-saved every turn",
        "세션은 매 턴 자동 저장됩니다",
        "セッションは毎ターン自動保存されます",
    ),
    ("Error: {e}", "오류: {e}", "エラー: {e}"),
    (
        "Error: the turn execution thread has terminated",
        "오류: 턴 실행 스레드가 종료되었습니다",
        "エラー: ターン実行スレッドが終了しました",
    ),
    ("modified: {}", "modified: {}", "modified: {}"),
    ("Waiting", "대기 중", "待機中"),
    ("Thinking", "생각 중", "思考中"),
    ("Running tools", "도구 실행 중", "ツール実行中"),
    ("Generating response", "응답 생성 중", "応答生成中"),
    (
        "💭 thinking… ({} chars)",
        "💭 생각 중… ({}자)",
        "💭 思考中… ({}文字)",
    ),
    (
        "💭 thinking ({} chars)",
        "💭 생각 ({}자)",
        "💭 思考 ({}文字)",
    ),
    (
        "  — Ctrl+T collapse",
        "  — Ctrl+T 접기",
        "  — Ctrl+T 折りたたみ",
    ),
    ("  — Ctrl+T expand", "  — Ctrl+T 펼치기", "  — Ctrl+T 展開"),
    // ── chat 진입·루프 ─────────────────────────────────
    (
        "bulti chat — interactive prompt loop (/exit or Ctrl-D to quit, /help for help)",
        "bulti chat — 대화형 프롬프트 루프 (/exit 또는 Ctrl-D 로 종료, /help 로 안내)",
        "bulti chat — 対話型プロンプトループ (/exit または Ctrl-D で終了、/help で案内)",
    ),
    (
        "Endpoint is not configured (register one first with `bulti endpoint add`)",
        "엔드포인트가 설정되지 않았습니다 (bulti endpoint add 로 먼저 등록하세요)",
        "エンドポイントが設定されていません (先に `bulti endpoint add` で登録してください)",
    ),
    (
        "⚠️  No endpoint is registered. Please configure an endpoint before starting.",
        "⚠️  등록된 엔드포인트가 없습니다. 대화를 시작하기 전에 엔드포인트를 설정해 주세요.",
        "⚠️  登録済みのエンドポイントがありません。会話を始める前にエンドポイントを設定してください。",
    ),
    (
        "    If you already have one, check with `bulti endpoint list` and activate it with `bulti endpoint use <name>`.",
        "    이미 등록된 엔드포인트가 있으면 `bulti endpoint list` 로 확인하고 `bulti endpoint use <이름>` 으로 활성화할 수 있습니다.",
        "    既に登録済みなら `bulti endpoint list` で確認し、`bulti endpoint use <名前>` で有効化できます。",
    ),
    (
        "Endpoint name (default: main): ",
        "엔드포인트 이름 (기본: main): ",
        "エンドポイント名 (既定: main): ",
    ),
    (
        "Endpoint URL (e.g. http://127.0.0.1:8084/v1): ",
        "엔드포인트 URL (예: http://127.0.0.1:8084/v1): ",
        "エンドポイント URL (例: http://127.0.0.1:8084/v1): ",
    ),
    ("Model name: ", "모델 이름: ", "モデル名: "),
    (
        "API key (optional, Enter to skip): ",
        "API 키 (선택, 없으면 Enter): ",
        "API キー (任意、Enter で省略): ",
    ),
    ("URL is required", "URL 은 필수입니다", "URL は必須です"),
    (
        "Model name is required",
        "모델 이름은 필수입니다",
        "モデル名は必須です",
    ),
    ("Input error: {e}", "입력 오류: {e}", "入力エラー: {e}"),
    (
        "EOF — configuration aborted",
        "EOF — 설정 중단",
        "EOF — 設定を中止",
    ),
    (
        "Registered and activated endpoint '{name}'.\n",
        "엔드포인트 '{name}' 을(를) 등록하고 활성화했습니다.\n",
        "エンドポイント '{name}' を登録して有効化しました。\n",
    ),
    (
        "Started a new session (session id: {id})",
        "새 세션을 시작합니다 (세션 id: {id})",
        "新しいセッションを開始しました (セッション id: {id})",
    ),
    (
        "Resuming session '{id}'.",
        "세션 '{id}' 을(를) 재개합니다.",
        "セッション '{id}' を再開します。",
    ),
    (
        "Session '{id}' not found.",
        "세션 '{id}' 을(를) 찾을 수 없습니다.",
        "セッション '{id}' が見つかりません。",
    ),
    (
        "Settings save failed: {e}",
        "설정 저장 실패: {e}",
        "設定の保存に失敗しました: {e}",
    ),
    (
        "Session save failed: {e}",
        "세션 저장 실패: {e}",
        "セッションの保存に失敗しました: {e}",
    ),
    (
        "Session restore failed for '{id}': {e}",
        "세션 '{id}' 복원 실패: {e}",
        "セッション '{id}' の復元に失敗しました: {e}",
    ),
    (
        "Changed model to '{model}'.",
        "모델을 '{model}' (으)로 변경했습니다.",
        "モデルを '{model}' に変更しました。",
    ),
    (
        "Changed model to '{model}' and reasoning effort to '{effort}'.",
        "모델을 '{model}' (으)로, reasoning effort '{effort}' (으)로 변경했습니다.",
        "モデルを '{model}' に、reasoning effort を '{effort}' に変更しました。",
    ),
    (
        "Changed model to '{model}'. (effort '{effort}' ignored — low|medium|high)",
        "모델을 '{model}' (으)로 변경했습니다. (effort '{effort}' 는 무시됨 — low|medium|high)",
        "モデルを '{model}' に変更しました。(effort '{effort}' は無視 — low|medium|high)",
    ),
    (
        "Usage: /effort <low|medium|high>",
        "사용법: /effort <low|medium|high>",
        "使い方: /effort <low|medium|high>",
    ),
    (
        "Set reasoning effort to '{effort}'.",
        "reasoning effort 를 '{effort}' (으)로 설정했습니다.",
        "reasoning effort を '{effort}' に設定しました。",
    ),
    (
        "You can exit with /exit or Ctrl-D. Press /help for available commands.",
        "종료는 /exit 또는 Ctrl-D, 도움말은 /help 입니다.",
        "終了は /exit または Ctrl-D、ヘルプは /help です。",
    ),
    (
        "Conversation ended.",
        "대화를 종료합니다.",
        "会話を終了します。",
    ),
    (
        "Ending conversation",
        "종료 명령 — 대화 종료",
        "終了コマンド — 会話を終了",
    ),
    (
        "SIGINT received — ending conversation",
        "SIGINT 수신 — 대화 종료",
        "SIGINT を受信 — 会話を終了",
    ),
    (
        "Input read error: {e}",
        "입력 읽기 오류: {e}",
        "入力の読み取りエラー: {e}",
    ),
    (
        "EOF (Ctrl-D) — ending conversation",
        "EOF(Ctrl-D) — 대화 종료",
        "EOF (Ctrl-D) — 会話を終了",
    ),
    // ── /help ─────────────────────────────────────────
    (
        "Available commands:",
        "사용 가능한 커맨드:",
        "利用可能なコマンド:",
    ),
    ("Internal commands:", "내부 명령:", "内部コマンド:"),
    (
        "Quit conversation (EOF)",
        "대화 종료 (EOF)",
        "会話を終了 (EOF)",
    ),
    ("Interrupt and quit", "중단 후 종료", "中断して終了"),
    // ── 미지원 커맨드 ───────────────────────────────────
    (
        "Unknown command '{line}'. Type /help to see available commands.{}",
        "지원하지 않는 커맨드 '{line}' 입니다. /help 를 입력해 사용 가능한 명령을 확인하세요.{}",
        "未知のコマンド '{line}' です。/help で利用可能なコマンドを確認してください。{}",
    ),
    (
        "\nSimilar commands: {list}",
        "\n유사한 커맨드: {list}",
        "\n類似のコマンド: {list}",
    ),
    // ── /model 목록 ─────────────────────────────────────
    (
        "Available models (from configured endpoints):",
        "사용 가능한 모델 (설정된 엔드포인트 기준):",
        "利用可能なモデル (設定済みエンドポイント基準):",
    ),
    (
        "  (no model configured — set one with /model <name>)",
        "  (모델이 설정되지 않았습니다 — /model <모델명> 으로 지정)",
        "  (モデルが設定されていません — /model <モデル名> で指定)",
    ),
    (" (current)", " (현재)", " (現在)"),
    (
        "Usage: /model <name> [low|medium|high]",
        "사용법: /model <모델명> [low|medium|high]",
        "使い方: /model <モデル名> [low|medium|high]",
    ),
    // ── /endpoint ─────────────────────────────────────
    (
        "Endpoint not found: {name}\n{list}",
        "엔드포인트를 찾을 수 없습니다: {name}\n{list}",
        "エンドポイントが見つかりません: {name}\n{list}",
    ),
    (
        "Endpoint not found: {name}",
        "엔드포인트를 찾을 수 없습니다: {name}",
        "エンドポイントが見つかりません: {name}",
    ),
    (
        "Endpoint: {name}{active}",
        "엔드포인트: {name}{active}",
        "エンドポイント: {name}{active}",
    ),
    (" (active)", " (활성)", " (有効)"),
    ("auto (probe)", "자동(프로브)", "自動 (プローブ)"),
    (
        "No endpoints registered.",
        "등록된 엔드포인트가 없습니다.",
        "登録済みのエンドポイントがありません。",
    ),
    ("Endpoint list:", "엔드포인트 목록:", "エンドポイント一覧:"),
    // ── /mcp ──────────────────────────────────────────
    (
        "MCP server not found: {name}\n{list}",
        "MCP 서버를 찾을 수 없습니다: {name}\n{list}",
        "MCP サーバーが見つかりません: {name}\n{list}",
    ),
    ("(no MCP server)", "(MCP 서버 없음)", "(MCP サーバーなし)"),
    ("MCP servers:", "MCP 서버:", "MCP サーバー:"),
    ("(no description)", "(설명 없음)", "(説明なし)"),
    (
        "MCP server: {name}",
        "MCP 서버: {name}",
        "MCP サーバー: {name}",
    ),
    ("  description: {d}", "  설명: {d}", "  説明: {d}"),
    // ── /session-info ─────────────────────────────────
    ("cwd: {cwd}", "cwd: {cwd}", "cwd: {cwd}"),
    ("Endpoint: {ep}", "엔드포인트: {ep}", "エンドポイント: {ep}"),
    ("Model: {m}", "모델: {m}", "モデル: {m}"),
    ("Turns: {n}", "턴 수: {n}", "ターン数: {n}"),
    (
        "Context usage: ~{est} / {ctx} tokens (~{pct}%)",
        "컨텍스트 사용량: ~{est} / {ctx} 토큰 (약 {pct}%)",
        "コンテキスト使用量: ~{est} / {ctx} トークン (約 {pct}%)",
    ),
    (
        "Context usage: ~{est} tokens (estimated — endpoint context_tokens not set)",
        "컨텍스트 사용량: ~{est} 토큰 (추정 — 엔드포인트 context_tokens 미설정)",
        "コンテキスト使用量: ~{est} トークン (推定 — エンドポイント context_tokens 未設定)",
    ),
    ("Created: {t}", "생성: {t}", "作成: {t}"),
    ("Last updated: {t}", "마지막 갱신: {t}", "最終更新: {t}"),
    // ── /sessions ─────────────────────────────────────
    (
        "No sessions.",
        "세션이 없습니다.",
        "セッションがありません。",
    ),
    (
        "Failed to list sessions: {e}",
        "세션 목록 조회 실패: {e}",
        "セッション一覧の取得に失敗しました: {e}",
    ),
    (
        "{id}  Turn={t}  Model={m}  Updated={u}\n",
        "{id}  턴={t}  모델={m}  갱신={u}\n",
        "{id}  ターン={t}  モデル={m}  更新={u}\n",
    ),
    // ── /resume ───────────────────────────────────────
    (
        "Specify a session id. Available sessions:",
        "세션 id 를 지정해 주세요. 사용 가능한 세션:",
        "セッション id を指定してください。利用可能なセッション:",
    ),
    (
        "  (no sessions)",
        "  (세션이 없습니다)",
        "  (セッションがありません)",
    ),
    (
        "\n  /resume {id}  (turn={t} model={m})",
        "\n  /resume {id}  (턴={t} 모델={m})",
        "\n  /resume {id}  (ターン={t} モデル={m})",
    ),
    (
        "\n  … and {n} more (full list: /sessions)",
        "\n  … 외 {n}개 (전체 목록: /sessions)",
        "\n  … 他 {n} 件 (全一覧: /sessions)",
    ),
    (
        "\n  (failed to list sessions: {e})",
        "\n  (세션 목록 조회 실패: {e})",
        "\n  (セッション一覧の取得に失敗: {e})",
    ),
    (
        "Session resume is not supported in the TUI — quit and resume with bulti chat --resume <id>.",
        "TUI 에서 세션 재개는 지원하지 않습니다 — 종료 후 bulti chat --resume <id> 로 재개하세요.",
        "TUI ではセッションの再開はできません — 終了後に bulti chat --resume <id> で再開してください。",
    ),
    (
        "Session redirect is not supported in the TUI for '{id}' (quit and resume with bulti chat --resume {id}).",
        "TUI 에서 세션 '{id}' 재개는 지원하지 않습니다 (종료 후 bulti chat --resume {id} 로 재개하세요).",
        "TUI ではセッション '{id}' の再開はできません (終了後に bulti chat --resume {id} で再開してください)。",
    ),
    (
        "/new is not supported in the TUI (quit and start again).",
        "/new 는 TUI 에서 지원하지 않습니다 (종료 후 다시 시작하세요).",
        "/new は TUI ではサポートされません (終了後に再起動してください)。",
    ),
    // ── /fork /export /compact ────────────────────────
    (
        "Forked the session. New session id: {id} (resume: /resume {id})",
        "세션을 분기했습니다. 새 세션 id: {id} (재개: /resume {id})",
        "セッションを分岐しました。新しいセッション id: {id} (再開: /resume {id})",
    ),
    (
        "Session fork failed: {e}",
        "세션 분기 실패: {e}",
        "セッション分岐に失敗しました: {e}",
    ),
    (
        "Exported conversation history to {path}.",
        "대화 기록을 {path} (으)로 내보냈습니다.",
        "会話履歴を {path} にエクスポートしました。",
    ),
    (
        "Export failed: {e}",
        "내보내기 실패: {e}",
        "エクスポートに失敗しました: {e}",
    ),
    (
        "Nothing to compact. (0 turns)",
        "컴팩트할 대화가 없습니다. (턴 0개)",
        "圧縮する会話がありません。(ターン 0)",
    ),
    (
        "Compacted the conversation into a summary.",
        "대화 기록을 요약으로 압축했습니다.",
        "会話履歴を要約に圧縮しました。",
    ),
    (
        "Compacted the conversation into a summary. (save failed: {e})",
        "대화 기록을 요약으로 압축했습니다. (저장 실패: {e})",
        "会話履歴を要約に圧縮しました。(保存失敗: {e})",
    ),
    (
        "Summary was empty; compaction cancelled. (existing session kept)",
        "요약이 비어 있어 컴팩트를 취소했습니다. (기존 세션 유지)",
        "要約が空のため圧縮を中止しました。(既存のセッションを維持)",
    ),
    (
        "Compaction failed (existing session kept): {e}",
        "컴팩트 실패 (기존 세션 유지): {e}",
        "圧縮に失敗しました (既存のセッションを維持): {e}",
    ),
    (
        "[compacted conversation summary]",
        "[컴팩트된 대화 요약]",
        "[圧縮された会話の要約]",
    ),
    (
        "Segment failed — previous conversation context is preserved",
        "세그먼트 실패 — 이전 대화 맥락은 유지됩니다",
        "セグメント失敗 — 以前の会話コンテキストは維持されます",
    ),
    (
        "Segment incomplete — previous conversation context is preserved",
        "세그먼트 미완료 — 이전 대화 맥락은 유지됩니다",
        "セグメント未完了 — 以前の会話コンテキストは維持されます",
    ),
    (
        "Summarize the conversation below concisely. Include all decisions, facts, changed files, and unresolved issues, but compress unnecessary dialogue. Output only the summary without extra explanation.\n\n[Conversation]\n",
        "아래 대화 기록을 간결하게 요약하세요. 결정·사실·수정된 파일·미해결 문제를 빠짐없이 담되 불필요한 대화는 압축하세요. 요약만 출력하고 다른 설명은 붙이지 마세요.\n\n[대화 기록]\n",
        "以下の会話を簡潔に要約してください。決定・事実・変更したファイル・未解決の問題を漏れなく含め、不要な会話は圧縮してください。要約のみを出力し、他の説明は付けないでください。\n\n[会話]\n",
    ),
    // ── /usage ────────────────────────────────────────
    (
        "Session total: ↑{in} ↓{out} ({turns} turns)",
        "세션 총합: ↑{in} ↓{out} (턴 {turns})",
        "セッション合計: ↑{in} ↓{out} ({turns} ターン)",
    ),
    ("Cost: ${amount}", "비용: ${amount}", "コスト: ${amount}"),
    (
        "Cost: — (input_price_per_mtok / output_price_per_mtok not set in bulti.toml)",
        "비용: — (bulti.toml 에 input_price_per_mtok·output_price_per_mtok 미설정)",
        "コスト: — (bulti.toml に input_price_per_mtok・output_price_per_mtok 未設定)",
    ),
    // ── /history ──────────────────────────────────────
    (
        "Prompt history (last 20):",
        "프롬프트 히스토리 (최근 20개):",
        "プロンプト履歴 (直近 20 件):",
    ),
    (
        "Prompt history (last 20) — you can also browse with ↑/↓ when the input is empty:",
        "프롬프트 히스토리 (최근 20개) — 빈 입력 상태에서 ↑/↓ 키로도 탐색할 수 있습니다:",
        "プロンプト履歴 (直近 20 件) — 入力が空のときは ↑/↓ キーでも閲覧できます:",
    ),
    (
        "Prompt history — results for '{q}' (last 20):",
        "프롬프트 히스토리 — '{q}' 검색 결과 (최근 20개):",
        "プロンプト履歴 — '{q}' の検索結果 (直近 20 件):",
    ),
    (
        "  (no history)",
        "  (히스토리가 없습니다)",
        "  (履歴がありません)",
    ),
    // ── /language ─────────────────────────────────────
    (
        "Language: {name} ({code})\nAvailable: en, ko, ja\nUsage: /language <en|ko|ja>",
        "언어: {name} ({code})\n사용 가능: en, ko, ja\n사용법: /language <en|ko|ja>",
        "言語: {name} ({code})\n利用可能: en, ko, ja\n使い方: /language <en|ko|ja>",
    ),
    (
        "Language changed to {name} ({code}).",
        "언어를 {name} ({code}) (으)로 변경했습니다.",
        "言語を {name} ({code}) に変更しました。",
    ),
    (
        "Unsupported language '{arg}'. Use en, ko, or ja.",
        "지원하지 않는 언어 '{arg}' 입니다. en, ko, ja 중 하나를 사용하세요.",
        "未対応の言語 '{arg}' です。en, ko, ja のいずれかを使用してください。",
    ),
    // ── run ───────────────────────────────────────────
    (
        "Endpoint '{name}' not found",
        "엔드포인트 '{name}' 이(가) 없습니다",
        "エンドポイント '{name}' がありません",
    ),
    (
        "Chain finished: {status}",
        "체인 종료: {status}",
        "チェーン終了: {status}",
    ),
    (
        "Segment {i} started (depth {d})",
        "세그먼트 {i} 시작 (depth {d})",
        "セグメント {i} 開始 (depth {d})",
    ),
    (
        "Segment {i} finished: {status}",
        "세그먼트 {i} 종료: {status}",
        "セグメント {i} 終了: {status}",
    ),
    (
        "max-handoff-depth reached — ending chain",
        "max-handoff-depth 도달 — 체인 종료",
        "max-handoff-depth に到達 — チェーンを終了",
    ),
    (
        "Chain status: {status}",
        "체인 상태: {status}",
        "チェーン状態: {status}",
    ),
    // ── endpoint 서브커맨드 ────────────────────────────
    (
        "Registered endpoint '{name}'",
        "엔드포인트 '{name}' 등록 완료",
        "エンドポイント '{name}' を登録しました",
    ),
    (
        "Activated endpoint '{name}'",
        "활성 엔드포인트를 '{name}' 로 전환했습니다",
        "有効なエンドポイントを '{name}' に切り替えました",
    ),
    (
        "Removed endpoint '{name}'",
        "엔드포인트 '{name}' 제거 완료",
        "エンドポイント '{name}' を削除しました",
    ),
    (
        "set must be in `key=value` form",
        "set 은 `key=value` 형태여야 합니다",
        "set は `key=value` 形式である必要があります",
    ),
    (
        "Endpoint '{name}' api_key unchanged",
        "엔드포인트 '{name}' api_key 변경 없음",
        "エンドポイント '{name}' の api_key は変更なし",
    ),
    (
        "Endpoint '{name}' {field} = {value}",
        "엔드포인트 '{name}' {field} = {value}",
        "エンドポイント '{name}' {field} = {value}",
    ),
    (
        "Endpoint '{name}' connection/auth succeeded",
        "엔드포인트 '{name}' 연결·인증 성공",
        "エンドポイント '{name}' の接続・認証に成功",
    ),
    (
        "⚠️  Endpoint '{name}' error {status}: {body}",
        "⚠️  엔드포인트 '{name}' 오류 {status}: {body}",
        "⚠️  エンドポイント '{name}' エラー {status}: {body}",
    ),
    (
        "Endpoint '{name}' context length: {tokens} (source: {source})",
        "엔드포인트 '{name}' 컨텍스트 길이: {tokens} (근거: {source})",
        "エンドポイント '{name}' のコンテキスト長: {tokens} (根拠: {source})",
    ),
    // ── session 서브커맨드 ─────────────────────────────
    ("ID", "ID", "ID"),
    ("Created", "생성 시각", "作成日時"),
    ("Turns", "턴수", "ターン数"),
    ("Updated", "갱신 시각", "更新日時"),
    (
        "Deleted session '{id}'.",
        "세션 '{id}' 을(를) 삭제했습니다.",
        "セッション '{id}' を削除しました。",
    ),
    // ── history 서브커맨드 ─────────────────────────────
    (
        "No recorded tasks.",
        "기록된 작업이 없습니다.",
        "記録されたタスクがありません。",
    ),
    (
        "id must be a number: {id}",
        "id 는 숫자여야 합니다: {id}",
        "id は数値である必要があります: {id}",
    ),
    ("Task #{id}\n", "작업 #{id}\n", "タスク #{id}\n"),
    (
        "  status:      {}",
        "  상태:        {}",
        "  状態:        {}",
    ),
    (
        "  started:     {}",
        "  시작:        {}",
        "  開始:        {}",
    ),
    (
        "  finished:    {}",
        "  종료:        {}",
        "  終了:        {}",
    ),
    (
        "  cwd:         {}",
        "  cwd:         {}",
        "  cwd:         {}",
    ),
    (
        "  endpoint:    {}",
        "  endpoint:    {}",
        "  endpoint:    {}",
    ),
    (
        "  model:       {}",
        "  model:       {}",
        "  model:       {}",
    ),
    (
        "  chain_id:    {}",
        "  chain_id:    {}",
        "  chain_id:    {}",
    ),
    (
        "  segment:     {}",
        "  segment:     {}",
        "  segment:     {}",
    ),
    (
        "  depth:       {}",
        "  depth:       {}",
        "  depth:       {}",
    ),
    (
        "  parent_run:  {}",
        "  parent_run:  {}",
        "  parent_run:  {}",
    ),
    (
        "  tokens:      {}/{}",
        "  tokens:      {}/{}",
        "  tokens:      {}/{}",
    ),
    (
        "  files:       {}",
        "  files:       {}",
        "  files:       {}",
    ),
    (
        "  duration:    {}ms",
        "  duration:    {}ms",
        "  duration:    {}ms",
    ),
    (
        "\n  prompt:\n{}",
        "\n  프롬프트:\n{}",
        "\n  プロンプト:\n{}",
    ),
    ("\n  result:\n{res}", "\n  결과:\n{res}", "\n  結果:\n{res}"),
    (
        "Task #{id} not found.",
        "작업 #{id} 을 찾을 수 없습니다.",
        "タスク #{id} が見つかりません。",
    ),
    (
        "Last task: #{id}\n",
        "마지막 작업: #{id}\n",
        "最後のタスク: #{id}\n",
    ),
    // ── skill/mcp/prompt/config ───────────────────────
    ("(no skills)", "(스킬 없음)", "(スキルなし)"),
    (
        "Global prompt edited.",
        "글로벌 프롬프트 편집 완료",
        "グローバルプロンプトを編集しました。",
    ),
    (
        "Editor exited with an error (exit {code}): {editor}",
        "편집기가 오류 상태로 종료되었습니다 (exit {code}): {editor}",
        "エディタがエラーで終了しました (exit {code}): {editor}",
    ),
    (
        "Failed to launch editor ({editor}): {e}",
        "편집기 실행 실패 ({editor}): {e}",
        "エディタの起動に失敗しました ({editor}): {e}",
    ),
    (
        "Config key not found: {key}",
        "설정 키를 찾을 수 없습니다: {key}",
        "設定キーが見つかりません: {key}",
    ),
    (
        "Unsupported config key for set: {key}",
        "설정 키 수정 미지원: {key}",
        "設定キーの変更は未対応: {key}",
    ),
    (
        "Settings load failed: {e}",
        "설정 로드 실패: {e}",
        "設定の読み込みに失敗しました: {e}",
    ),
    ("Execution error: {e}", "실행 오류: {e}", "実行エラー: {e}"),
    // ── 프롬프트 인덱스 ────────────────────────────────
    (
        "## Lazy-loading index (skills · MCP · history)",
        "## 레이지 로딩 인덱스 (스킬·MCP·history)",
        "## 遅延読み込みインデックス (スキル・MCP・history)",
    ),
    ("- Skills: none\n", "- 스킬: 없음\n", "- スキル: なし\n"),
    (
        "- Skills (load on demand with `skill_load(name)`):\n",
        "- 스킬 (필요할 때 `skill_load(name)` 로 로드):\n",
        "- スキル (必要なとき `skill_load(name)` で読み込み):\n",
    ),
    (
        "- MCP servers: none\n",
        "- MCP 서버: 없음\n",
        "- MCP サーバー: なし\n",
    ),
    (
        "- MCP servers (load on demand with `mcp_tools(server)`):\n",
        "- MCP 서버 (필요할 때 `mcp_tools(server)` 로 로드):\n",
        "- MCP サーバー (必要なとき `mcp_tools(server)` で読み込み):\n",
    ),
    (
        "- history (recall past task context): `history_list(query?, limit?)`, `history_read(run_id)`\n",
        "- history (이전 작업 맥락 회수): `history_list(query?, limit?)`, `history_read(run_id)`\n",
        "- history (過去のタスク文脈の参照): `history_list(query?, limit?)`, `history_read(run_id)`\n",
    ),
    ("(none)", "(없음)", "(なし)"),
];

/// `key`(영어 원문)를 현재 언어로 번역한다. 번역이 없으면 `key` 를 그대로 반환한다.
pub fn tr(key: &'static str) -> &'static str {
    translate(current(), key)
}

/// `key`(영어 원문)를 지정 언어로 번역한다. 테스트·명시적 언어 지정용.
pub fn translate(lang: Language, key: &'static str) -> &'static str {
    match lang {
        Language::En => key,
        Language::Ko => lookup(key, 1),
        Language::Ja => lookup(key, 2),
    }
}

/// 카탈로그에서 `key` 에 해당하는 인덱스 필드를 찾는다. 없으면 `key`(영어) 반환.
fn lookup(key: &'static str, field: usize) -> &'static str {
    for (en, ko, ja) in CATALOG {
        if *en == key {
            return if field == 1 { ko } else { ja };
        }
    }
    key
}

/// `key`(영어 원문)를 현재 언어로 번역하고 `{}` 자리표시자를 `args` 로 순서대로
/// 치환한 문자열을 만든다. 인자가 부족하면 해당 자리표시자는 빈 문자열이 된다.
///
/// `format!` 은 컴파일 타임 리터럴을 요구하므로 런타임 번역 문자열에는 이
/// 함수를 쓴다.
pub fn tr_fmt(key: &'static str, args: &[&str]) -> String {
    translate_fmt(current(), key, args)
}

/// [`tr_fmt`] 의 명시적 언어 버전. 테스트용.
///
/// `{}`·`{name}` 등 중괄호로 감싼 자리표시자를 순서대로 `args` 로 치환한다.
/// 번역문에서도 자리표시자 순서가 영어 원문과 같아야 한다.
pub fn translate_fmt(lang: Language, key: &'static str, args: &[&str]) -> String {
    let tmpl = translate(lang, key);
    let mut out = String::with_capacity(tmpl.len() + 16);
    let mut rest = tmpl;
    let mut arg_idx = 0usize;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                if let Some(a) = args.get(arg_idx) {
                    out.push_str(a);
                }
                arg_idx += 1;
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_language_is_english() {
        assert_eq!(Language::default(), Language::En);
        assert_eq!(translate(Language::En, "Switch model"), "Switch model");
    }

    #[test]
    fn korean_translation_lookup() {
        assert_eq!(translate(Language::Ko, "Switch model"), "모델 전환");
    }

    #[test]
    fn japanese_translation_lookup() {
        assert_eq!(translate(Language::Ja, "Switch model"), "モデルの切り替え");
    }

    #[test]
    fn unknown_key_falls_back_to_english() {
        assert_eq!(
            translate(Language::Ko, "some unknown key"),
            "some unknown key"
        );
    }

    #[test]
    fn translate_fmt_substitutes_placeholders() {
        assert_eq!(
            translate_fmt(Language::En, "Usage: /effort <low|medium|high>", &[]),
            "Usage: /effort <low|medium|high>"
        );
        assert_eq!(
            translate_fmt(Language::Ko, "Error: {e}", &["boom"]),
            "오류: boom"
        );
        assert_eq!(
            translate_fmt(Language::Ko, "Usage: /model <name> [low|medium|high]", &[]),
            "사용법: /model <모델명> [low|medium|high]"
        );
        assert_eq!(
            translate_fmt(Language::En, "Changed model to '{model}'.", &["gpt-4"]),
            "Changed model to 'gpt-4'."
        );
    }

    #[test]
    fn global_set_and_current_roundtrip() {
        set_language(Language::Ko);
        assert_eq!(current(), Language::Ko);
        set_language(Language::En);
        assert_eq!(current(), Language::En);
    }

    #[test]
    fn language_codes_roundtrip() {
        for lang in Language::ALL {
            assert_eq!(Language::from_code(lang.code()), Some(lang));
        }
        assert_eq!(Language::from_code("EN"), Some(Language::En));
        assert_eq!(Language::from_code("kr"), Some(Language::Ko));
        assert_eq!(Language::from_code("xx"), None);
    }

    #[test]
    fn every_catalog_entry_has_all_translations() {
        for (en, ko, ja) in CATALOG {
            assert!(!en.is_empty(), "empty english key");
            assert!(!ko.is_empty(), "empty korean for {en}");
            assert!(!ja.is_empty(), "empty japanese for {en}");
        }
    }

    #[test]
    fn catalog_keys_are_unique() {
        for (i, (en, _, _)) in CATALOG.iter().enumerate() {
            for (other, _, _) in CATALOG.iter().skip(i + 1) {
                assert_ne!(en, other, "duplicate catalog key: {en}");
            }
        }
    }
}
