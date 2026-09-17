# bulti

[English](README.md) · **한국어**

> **bulti**(불티)는 **컨텍스트 핸드오프 체인**으로 긴 작업을 끝까지 완결하는, Rust로
> 작성한 로컬 LLM 전용 코딩 에이전트 CLI입니다.
> 사용자의 컴퓨터에서 직접 구동되는 OpenAI 호환 추론 서버(llama.cpp, vLLM, Ollama,
> LM Studio)만 엔드포인트로 사용하며, 원격 유료 API는 지원하지 않습니다.
> 컨텍스트 창이 부족해지면 작업을 9개 섹션으로 요약해 새 세그먼트로 넘기므로, 작은
> 로컬 모델이라도 큰 작업을 끝까지 완수할 수 있습니다.

*로컬 모델을 위한 단일 바이너리 코딩 에이전트입니다. 컨텍스트 한계와 싸우는 대신,
한계에 도달하기 전에 작업을 넘깁니다. 엔드포인트 등록, 컨텍스트 길이 프로브, 레이지
스킬·MCP, 자동 히스토리 데이터베이스, 채팅/TUI, 단발 오케스트레이션이 모두 내장되어
있습니다. 데몬도 서버도 클라우드도 필요하지 않습니다.*

![Rust](https://img.shields.io/badge/Rust-edition%202024-dea584?logo=rust&logoColor=white)
![License](https://img.shields.io/badge/license-MIT-blue)
![Backend](https://img.shields.io/badge/backend-OpenAI%20compatible-6e56cf)
![Mode](https://img.shields.io/badge/mode-local%20LLM%20only-orange)

불티가 겨냥하는 문제는 모델의 지능 자체가 아니라 **로컬 모델 컨텍스트 창의 물리적
한계**입니다. 설계는 이 문제를 하나의 메커니즘으로 해결합니다. 바로
[컨텍스트 핸드오프 체인](#컨텍스트-핸드오프-체인)입니다. 프로브 체인, 레이지 로딩,
히스토리 데이터베이스, 가드 체계는 모두 이 핸드오프를 깨끗하게 유지하기 위해
존재합니다. 단발 `bulti run`은 오케스트레이션 계약(exit code + `--json`)이고, 대화형
`bulti chat`은 일상적인 사용을 위한 진입점(세션 + TUI)입니다. 전체 설계는
[`DESIGN.md`](DESIGN.md)에 있습니다.

## 목차

- [불티가 존재하는 이유](#불티가-존재하는-이유)
- [한눈에 보기](#한눈에-보기)
- [주요 기능](#주요-기능)
- [요구 사항](#요구-사항)
- [설치](#설치)
- [빠른 시작](#빠른-시작)
- [대화형 모드 (`bulti chat`)](#대화형-모드-bulti-chat)
- [단발 모드와 오케스트레이션 (`bulti run`)](#단발-모드와-오케스트레이션-bulti-run)
- [컨텍스트 핸드오프 체인](#컨텍스트-핸드오프-체인)
- [엔드포인트와 컨텍스트 길이 프로브](#엔드포인트와-컨텍스트-길이-프로브)
- [작업 히스토리](#작업-히스토리)
- [스킬 (레이지 로딩)](#스킬-레이지-로딩)
- [MCP (레이지 로딩, 2단계)](#mcp-레이지-로딩-2단계)
- [시스템 프롬프트](#시스템-프롬프트)
- [네이티브 도구](#네이티브-도구)
- [가드 체계](#가드-체계)
- [설정](#설정)
- [자동 업데이트](#자동-업데이트)
- [아키텍처](#아키텍처)
- [개발](#개발)
- [라이선스](#라이선스)

## 불티가 존재하는 이유

로컬 모델은 저렴하고 사적이며 언제나 사용할 수 있습니다. 그러나 컨텍스트 창이 작고
추론이 불안정합니다. 기존 방식의 에이전트를 로컬 엔드포인트에 연결하면 매번 같은
실패가 반복됩니다. 작업 도중에 컨텍스트가 가득 차고, 아무것도 이어받지 못한 채
실행이 종료됩니다. "그냥 대화를 계속 이어 가면 되지 않느냐"는 시도는 조용히 대화의
앞부분을 버리며, 모델은 보이지 않는 내용을 환각으로 채우기 시작합니다.

불티는 반대의 입장을 취합니다. 즉 컨텍스트를 **희소 자원**으로 취급합니다.

1. **미리 넣는 정보를 최소화합니다.** 스킬과 MCP 서버는 시스템 프롬프트에 이름과
   설명의 인덱스만 넣고, 본문과 스키마는 필요할 때 도구로 불러옵니다.
2. **세션은 저장하되 작업은 잇습니다.** 대화형 모드는 세션을 저장하고 재개합니다.
   모든 실행은 작업(run) 단위로 기록되며, 컨텍스트가 부족해지면 작업을 요약해 새
   세그먼트로 자동으로 넘깁니다.

그 결과로 탄생한 단일 바이너리는 꼼꼼한 동료처럼 동작합니다. 편집하기 전에 읽고,
완료를 선언하기 전에 검증하며, 공간이 부족해지면 정교한 인수인계 문서를 작성한 뒤
작업을 다시 이어 갑니다.

## 한눈에 보기

```
        외부 오케스트레이터 (셸 스크립트 / CI / 다른 에이전트)
                 │  bulti run "프롬프트" --json   (exit 0/1/2/130)
                 ▼
 ┌────────────────────────── 단일 bulti 바이너리 ─────────────────────────┐
 │                                                                        │
 │  cli(clap) ── config(~/.bulti/config.toml) ── endpoint(프로브 · n_ctx)  │
 │      │                                                                 │
 │      ├── bulti run  ──▶ agent::run  ── 핸드오프 체인 (세그먼트 1→2→…→N) │
 │      │      │           │ 요약 요청 → ===NEXT_TASK=== → 새 세그먼트     │
 │      │      │           ▼                                             │
 │      │      │     세그먼트 루프 ── llm(SSE 클라이언트) ──▶ 로컬 추론 서버│
 │      │      │           │ ▲                                           │
 │      │      │           ▼ │                                           │
 │      │      │     tools 레지스트리 ── bash / read_file / write_file /  │
 │      │      │            edit_file / glob / grep (정의+실행 통합)       │
 │      │      │            / skill_load · mcp_tools · mcp_call (레이지)  │
 │      │      │            / history_list · history_read                │
 │      │      │                                                         │
 │      ├── bulti chat ──▶ agent::session ── 대화형 프롬프트 루프         │
 │      │      │           (다중 턴 + 세션 저장·재개)                      │
 │      │      │           동일 코어 · 동일 tools 레지스트리 공유          │
 │      │      │                                                         │
 │      ├── context(토큰 추정 · 트리밍 · 절단) · guards(퇴행 방어)         │
 │      ├── history(SQLite: run · 세션 · 체인 자동 기록)                   │
 │      ├── prompt(계층 조립: 빌트인 + 글로벌 + 프로젝트 + 인덱스)          │
 │      └── update(GitHub Releases → self-replace)                       │
 └────────────────────────────────────────────────────────────────────────┘
```

- **두 진입점, 하나의 코어.** 자동화를 위한 `bulti run`과 사람을 위한 `bulti chat`은
  에이전트 루프, 핸드오프 체인, 스킬·MCP, 히스토리를 공유합니다.
- **단일 바이너리.** 데몬도 서버 프로세스도 없으며, `~/.bulti` 외에 런타임 파일을
  만들지 않습니다.

## 주요 기능

- **컨텍스트 핸드오프 체인** — 핵심 기능입니다. 컨텍스트 창의 75%에 도달하면 에이전트가
  9개 섹션으로 구성된 구조화 요약과 `===NEXT_TASK===` 마커를 작성한 뒤, 그 요약을
  출발점으로 새 세그먼트를 시작합니다. 체인은 `chain_id`로 식별하며 깊이 상한은
  12입니다.
- **두 가지 진입점**
  - `bulti chat` (서브커맨드를 생략했을 때의 기본값) — 세션 저장·재개를 지원하는
    대화형 다중 턴 채팅/TUI입니다.
  - `bulti run` — exit code 계약과 `--json` 보고서를 갖춘, 한 번의 호출로 완결되는
    단발 자동화입니다.
- **OpenAI 호환 엔드포인트 관리** — llama.cpp / vLLM / Ollama / LM Studio 서버를
  등록하고, 하나를 활성화하며, 연결을 테스트하고, 컨텍스트 길이를 프로브합니다.
- **컨텍스트 길이 프로브 체인** — 수동 설정 → `GET /v1/models`(`max_model_len`,
  `meta.n_ctx`, `max_context_length`) → `GET /props`(llama.cpp) →
  `GET /api/show`(Ollama) → 폴백 32768 순으로 결정하며, HTTP 400을 받으면 런타임에
  자동 교정합니다.
- **네이티브 도구** — `bash`, `read_file`, `write_file`, `edit_file`, `glob`, `grep`,
  `history_list`, `history_read`, `skill_load`, `mcp_tools`, `mcp_call`.
- **레이지 스킬·MCP** — 시스템 프롬프트에는 인덱스만 들어갑니다. 본문과 도구 스키마는
  필요할 때 가져오므로, 수십 개의 MCP 도구가 컨텍스트 예산을 잠식하지 않습니다.
- **자동 히스토리** — 모든 실행이 SQLite(`~/.bulti/history.db`)에 기록되며, CLI와
  모델 자신이 모두 조회할 수 있습니다.
- **계층형 시스템 프롬프트** — 빌트인 베이스 + `~/.bulti/prompts/default.md` +
  `<프로젝트>/.bulti/system.md` + 항상 존재하는 인덱스 섹션으로 조립하며, 전체 교체
  오버라이드도 지원합니다.
- **추론(reasoning) 지원** — `reasoning_content`를 스트리밍으로 표시(💭)하고 기록하되,
  다음 요청에는 다시 포함하지 않습니다.
- **다국어(i18n)** — 영어(기본)·한국어·일본어 UI를 지원하며, `/language`로 실행 중에
  바꿀 수 있습니다.
- **자동 업데이트** — GitHub Releases와의 semver 비교, sha256 검증, exit-time
  self-replace를 수행합니다.
- **가드 체계** — 빈 응답 루프, 스트림 반복, stuck 도구 시그니처, U+FFFD 퇴행, 거짓
  완료, 런어웨이 핸드오프를 막는 테이블 테스트 기반 방어층입니다.

## 요구 사항

| 항목 | 값 |
|------|-----|
| Rust | edition 2024 (rustc 1.85 이상) |
| 운영체제 | Linux, macOS (Rust 툴체인이 있는 플랫폼) |
| LLM 백엔드 | OpenAI 호환 로컬 서버 (llama.cpp, vLLM, Ollama, LM Studio 등) |
| 네트워크 | 선택적 업데이트 확인을 위한 `api.github.com` 아웃바운드 HTTPS만 필요 |

불티에는 **클라우드 요구 사항이 없습니다.** `reqwest`가 `rustls`를 사용하므로 시스템
OpenSSL에 의존하지 않으며, `rusqlite`가 번들되어 있어 시스템 SQLite도 필요하지
않습니다.

## 설치

### 소스에서 빌드

```bash
git clone https://github.com/agurrrrr/bulti.git
cd bulti
cargo build --release
# 바이너리는 target/release/bulti 에 있습니다.
install -m 0755 target/release/bulti ~/.local/bin/bulti
```

edition 2024는 Rust 1.85 이상을 요구합니다. 저장소의 포매팅 규칙은
[`rustfmt.toml`](rustfmt.toml)에 정의되어 있습니다(최대 너비 100, 공백 4칸, Unix 줄바꿈).

### 미리 빌드된 바이너리

릴리즈 아티팩트는 대상 트리플별 정적 musl 바이너리로 게시됩니다(예:
`bulti-x86_64-unknown-linux-musl.tar.gz`). `bulti update`는 실행 중인 바이너리의 대상
트리플과 일치하는 아티팩트를 선택하고, `checksums.txt` 아티팩트가 있으면 sha256
체크섬을 검증한 뒤, 프로세스 종료 시점에 실행 파일을 교체합니다.

### 설치 확인

```bash
bulti version
bulti --help
```

## 빠른 시작

### 1. 로컬 추론 서버를 시작합니다

OpenAI 호환이면 어떤 서버든 사용할 수 있습니다. llama.cpp를 사용하는 경우는 다음과
같습니다.

```bash
llama-server -m /path/to/model.gguf --port 8084 --ctx-size 32768 --no-context-shift
```

`--no-context-shift` 옵션을 권장합니다. 컨텍스트를 초과했을 때 오류를 내지 않고 조용히
앞부분을 자르는 서버는 클라이언트가 감지할 수 없으며, 모델의 출력을 퇴행시킵니다.

### 2. 엔드포인트를 등록합니다

```bash
bulti endpoint add main \
  --url http://127.0.0.1:8084/v1 \
  --model qwen3.8-27b-q2

bulti endpoint use main
bulti endpoint test main     # 연결성과 인증을 확인합니다.
bulti endpoint probe main    # 컨텍스트 길이와 그것을 확정한 출처를 확인합니다.
```

`context_tokens`의 기본값은 `0`이며, 이는 "매 실행마다 자동으로 프로브한다"는 뜻입니다.
프로브가 신뢰할 수 없을 때는 값을 명시적으로 설정하십시오.

```bash
bulti endpoint set main context_tokens=32768
```

### 3. 대화합니다

```bash
# 단발: 작업 하나를 완결하고 종료합니다.
bulti run "README.md 파일을 읽고 핵심 기능 3가지를 요약해줘"

# 대화형: 다중 턴 채팅/TUI (서브커맨드가 없을 때의 기본 동작이기도 합니다).
bulti chat
bulti                          # `bulti chat` 과 같습니다.
```

### 4. 오케스트레이션에 사용합니다

```bash
echo "오늘 변경 사항을 요약해줘" | bulti run - --json --quiet \
  | jq -e '.status == "completed"'
```

## 대화형 모드 (`bulti chat`)

`bulti chat`(또는 서브커맨드 없이 `bulti`)은 대화형 세션을 시작합니다. TTY에서는
ratatui TUI를 사용하며, `--no-tui`를 주면 일반 스트리밍 텍스트 루프로 전환합니다.

```bash
bulti chat [--endpoint NAME] [--model M] [--system-file F] [--system "TEXT"]
           [--resume <session_id>] [--no-tui] [--no-color] [--first "PROMPT"]
```

한 턴(사용자 프롬프트 하나 → 모델의 완료 응답 하나)은 `agent::session`이 주관하며,
내부적으로는 `bulti run`과 같은 세그먼트 루프를 실행합니다. 한 턴이 컨텍스트 한계에
도달하면 핸드오프 체인으로 이어 갑니다.

### TUI

- 어시스턴트 출력, 도구 호출(`🔧 이름 → 인자`), 도구 결과를 스트리밍으로 표시합니다.
- 추론(💭)을 표시하며 `Ctrl+T`로 접고 펼칠 수 있습니다.
- 상태 줄에 엔드포인트, 모델, 토큰 사용량, 세션 id, 경과 시간을 표시합니다.
- 멀티라인 입력을 항상 사용할 수 있으며(Shift+Enter), 빈 줄에서도 블록 커서와 실제
  터미널 커서가 캐럿을 표시합니다.
- 입력창이 자동으로 줄바꿈되며, 커서를 글자·단어·Home/End·`Ctrl+A`/`Ctrl+E`·`Delete`
  단위로 이동할 수 있습니다.
- 슬래시 커맨드 자동완성이 명령과 인자(엔드포인트, 모델, MCP 서버, 언어)를 제안합니다.
- `↑`/`↓` 방향키로 이전 프롬프트를 탐색합니다.

### 슬래시 커맨드

| 커맨드 | 별칭 | 설명 |
|--------|------|------|
| `/exit` | `/quit`, `/q` | 대화를 종료합니다 |
| `/new` | | 새 세션을 시작합니다 |
| `/help` | `/?` | 커맨드 도움말을 표시합니다 |
| `/resume <id>` | | 세션을 재개합니다 |
| `/model <name> [effort]` | `/m` | 모델을 전환합니다(추론 강도도 함께 설정 가능) |
| `/effort <low\|medium\|high>` | `/e` | 추론 강도를 설정합니다 |
| `/endpoint [name \| add\|set\|use\|remove …]` | `/ep` | 엔드포인트를 조회하거나 설정합니다 |
| `/mcp [name \| add\|set\|remove …]` | | MCP 서버를 조회하거나 등록합니다 |
| `/language <en\|ko\|ja>` | `/lang`, `/l` | UI 언어를 바꿉니다 |
| `/session-info` | `/info` | 세션 id / 모델 / 컨텍스트 사용량을 표시합니다 |
| `/sessions` | `/ls` | 세션 목록을 표시합니다 |
| `/compact` | | 대화 기록을 요약해 컨텍스트를 줄입니다 |
| `/fork` | | 현재 세션을 새 id로 복제합니다 |
| `/export [path]` | | 대화를 Markdown 파일로 내보냅니다 |
| `/usage` | `/u` | 세션 토큰과 비용 사용량을 표시합니다 |
| `/history [query]` | `/h` | 프롬프트 기록을 조회합니다 |

### 세션

세션은 `~/.bulti/sessions/<session_id>.json`에 저장합니다. 매 턴마다 메시지 배열이
갱신되며, `--resume <id>`(또는 `/resume`)로 복원합니다. 세션 파일은 대화 재개를 위한
원본이고, 히스토리 데이터베이스는 `session_id`로 연결되는 작업 단위 감사 기록입니다.

```bash
bulti session list
bulti session delete <id>
bulti chat --resume <id>
```

## 단발 모드와 오케스트레이션 (`bulti run`)

`bulti run`은 작업 하나를 한 번의 커맨드로 완결합니다. 도구 실행 승인을 요구하지
않으므로 스크립트, CI 잡, 다른 에이전트에서 호출하기에 안전합니다. 진행 출력은
stderr로 나가며, `--json`을 주면 최종 보고서가 stdout으로 정확히 한 번 나갑니다.

```bash
bulti run "PROMPT" [--endpoint NAME] [--model M]
                  [--system-file F] [--system "TEXT"]
                  [--json] [--quiet] [--no-color]
                  [--max-time SECONDS] [--max-handoff-depth N]
```

- `bulti run -`는 표준 입력 전체를 프롬프트로 읽습니다(파이프와 heredoc을 지원합니다).
- stderr가 TTY가 아니면 진행 출력이 자동으로 최소화됩니다.

### exit code 계약

| 코드 | 의미 | run 상태 |
|------|------|----------|
| 0 | 체인 완료 | `completed` |
| 1 | 실패 (엔드포인트 오류, 치명 버그) | `failed` |
| 2 | 미완료 종료 (가드, depth 한계, max-time 초과) | `incomplete` |
| 130 | SIGINT | `interrupted` |

### `--json` 보고서

```json
{
  "version": "0.2.0",
  "status": "completed",
  "chain_id": "0f9c…",
  "segments": 3,
  "handoff_depth": 2,
  "endpoint": "main",
  "model": "qwen3.8-27b-q2",
  "input_tokens": 81234,
  "output_tokens": 9412,
  "duration_ms": 331000,
  "files_touched": ["src/main.rs", "src/agent/loop_.rs"],
  "result": "최종 세그먼트의 완료 응답 텍스트",
  "runs": [1, 2, 3]
}
```

바로 복사해 쓸 수 있는 패턴이 [`examples/`](examples/)에 있습니다. 셸 파이프라인, CI
잡, 다른 에이전트의 서브프로세스 호출 예제를 제공합니다.

## 컨텍스트 핸드오프 체인

이것이 불티의 심장입니다.

```
[매 요청 직전] estimate(messages) ≥ context_tokens × handoff_threshold_pct (75%)
      │
      ▼
attempt_handoff: 도구 없이 마지막 요청 — 9섹션 구조화 요약 + ===NEXT_TASK=== 지시
      │
      ├─ 품질 게이트 통과
      │     ├─ NEXT_TASK 있음 ─▶ 현재 세그먼트를 완료 처리
      │     │                    요약+과제로 새 세그먼트 시작 (fresh messages)
      │     └─ NEXT_TASK 없음 ─▶ 체인 전체 완료 (exit 0)
      └─ 게이트 실패 / 요청 실패 ─▶ trim 폴백으로 현재 세그먼트를 계속 (다음 턴에 재시도)

handoff_depth ≥ warn(8)  ─▶ stderr 경고
handoff_depth ≥ max(12)  ─▶ 런어웨이 가드: 이후 핸드오프 금지 → incomplete (exit 2)
```

- 새 세그먼트의 프롬프트는 요약 전문과 `===NEXT_TASK===` 아래의 과제로 구성합니다.
  다음 세그먼트는 이전 대화를 볼 수 없으므로, 지시문은 파일 경로·결정 사항·주의점을
  모두 명시하도록 요구합니다.
- 9개 섹션은 원 요청/의도, 핵심 기술/개념, 열람·변경 파일, 한 일, 실패·수정, 현재
  진행, 남은 작업, 하지 말 것, 다음 한 걸음입니다.
- 품질 게이트는 최소 200자, 필수 섹션 키워드 5개 이상, degenerate 내용 검사를
  요구합니다. 핸드오프가 실패하면 실행을 잃지 않고 가장 오래된 턴을 자르는 폴백으로
  이어 갑니다.
- run의 최종 상태는 마지막 세그먼트가 아니라 **체인 전체**로 판정합니다. 어느
  세그먼트든 실패하거나 미완료로 끝나면 run이 그 상태를 물려받습니다.

## 엔드포인트와 컨텍스트 길이 프로브

컨텍스트 길이는 모든 핸드오프의 기준값이므로 가장 먼저 확정합니다. 우선순위는
다음과 같습니다.

1. **수동 설정** — `context_tokens > 0`이면 그 값을 최우선으로 사용합니다.
2. **`GET {base}/models`** — `data[].max_model_len`(vLLM), `data[].meta.n_ctx`
   (llama.cpp), `data[].max_context_length`(LM Studio)를 차례로 탐색합니다.
3. **`GET {root}/props`** (llama.cpp) — `default_generation_settings.n_ctx`를 읽습니다.
   URL이 `/v1`로 끝나면 상위 경로를 시도합니다.
4. **`GET {host}/api/show?model=<id>`** (Ollama) — `model_info`의 `*.context_length`를
   읽습니다.
5. **폴백 32768** — stderr 경고를 출력합니다.

프로브 결과는 캐시하지 않습니다. 서버 재시작으로 `n_ctx`가 바뀔 수 있으므로 매 실행
시작 시 다시 확정합니다. 컨텍스트 초과로 400 응답이 오면 오류에서 숫자를 파싱해
엔드포인트 설정을 자동으로 교정하고 경고를 남깁니다.

**시크릿 처리.** API 키는 `config.toml`(권장 권한 600)에 저장하며 출력은 항상
마스킹합니다. `endpoint set`에서 키 필드가 비어 있거나 마스킹 문자열 그대로면
"변경 없음"으로 처리해 실제 키를 덮어쓰지 않습니다.

```bash
bulti endpoint add|list|use|remove|set|test|probe
```

## 작업 히스토리

모든 실행은 `~/.bulti/history.db`(SQLite, 번들)에 자동으로 기록되며, 사용자가 끌 수
없습니다. 세션을 재사용하지 않는 단발 모드에서 과거 맥락을 공급하는 유일한 경로입니다.

```bash
bulti history list [-n N] [--status S] [--chain ID]
bulti history show <id>
bulti history last [--chain]
```

모델도 `history_list`와 `history_read` 도구로 조회할 수 있습니다. 단발 모드에서
"이전 작업을 이어서"가 동작하는 방식이 바로 이것입니다. 즉 모델이 스스로 관련 맥락을
회수하며, 시스템 프롬프트에는 도구가 존재한다는 사실만 한 줄로 안내합니다.

## 스킬 (레이지 로딩)

- **발견 순서:** 프로젝트 `<root>/.bulti/skills/` → 글로벌 `~/.bulti/skills/`.
  동명이면 프로젝트가 우선합니다.
- **형식:** YAML frontmatter(`name`, `description`)를 가진 Markdown이며, 단일 파일
  `<name>.md` 또는 리소스를 동반하는 디렉터리 `<name>/SKILL.md`입니다.
- **인덱스만 주입합니다:** 스킬 이름과 설명을 한 줄씩만 넣습니다. 본문은 절대 자동으로
  주입하지 않습니다.
- **`skill_load(name)`** 은 모델이 필요하다고 판단할 때 본문 전체를 반환합니다.
- 사용 예시로 번들 스킬 두 개(`commit-message`, `korean-report`)를 포함합니다.

```bash
bulti skill list
bulti skill show <name>
```

## MCP (레이지 로딩, 2단계)

shepherd와 달리 불티는 MCP 도구 스키마를 프롬프트에 자동 주입하지 않습니다. 수십 개의
스키마는 로컬 모델의 컨텍스트 예산에 치명적이기 때문입니다.

- **설정:** `config.toml`의 `[mcp.<name>]`(`command`, `args`, `env`, `description`).
- **서버 인덱스만 주입합니다:** 이름과 설명만 넣습니다.
- **2단계 로딩:**
  1. `mcp_tools(server)`가 도구 목록(이름, 설명, 파라미터 요약)을 반환합니다. 이
     시점부터 해당 서버의 스키마를 이후 요청에 옵트인 주입합니다. 모델이 요청했으므로
     레이지 원칙에 어긋나지 않습니다. 정의와 디스패처를 함께 활성화합니다.
  2. `mcp_call(server, tool, args)`가 도구를 호출합니다. 스키마 불일치 오류에는 해당
     파라미터 스키마를 다시 안내합니다.
- **클라이언트:** `rmcp` stdio transport를 사용합니다. 서버 프로세스는 첫 MCP 도구
  호출 시에만 spawn합니다.
- **결과 파싱:** `content`(text)와 `structuredContent`를 모두 고려하며, text가 비면
  structuredContent 원본 JSON으로 폴백합니다.
- 타임아웃(기본 60초)과 서버 장애는 도구 결과로 오류를 반환할 뿐 run을 죽이지
  않습니다.

```bash
bulti mcp list                 # CLI: 설정된 서버 목록
# 채팅 안에서는 /mcp [name | add|set|remove …]
```

## 시스템 프롬프트

조립 방식은 예측 가능한 계층 합성입니다.

```
[빌트인 베이스]                      # 정체성, 도구 규칙, 완료 규칙, 핸드오프 협력 규칙
+ [~/.bulti/prompts/default.md]      # 글로벌 추가 지시 (있으면)
+ [<프로젝트>/.bulti/system.md]      # 프로젝트 추가 지시 (있으면)
+ [인덱스 섹션]                       # 항상 자동: 스킬 목록, MCP 서버 목록, history 도구 안내
```

- **전체 교체:** `--system-file <path>` 또는 `--system "<text>"`를 주면 빌트인·글로벌·
  프로젝트 계층을 모두 무시하고 주어진 내용으로 교체합니다. 인덱스 섹션(스킬·MCP·
  history)은 유지합니다. 그렇지 않으면 레이지 로딩 안내가 사라지기 때문입니다.
- **템플릿 변수**(모든 계층에서 치환): `{{cwd}}`, `{{os}}`, `{{endpoint}}`,
  `{{model}}`, `{{context_tokens}}`.
- 빌트인 베이스는 [`src/prompt/base.md`](src/prompt/base.md)에 있고 `include_str!`로
  포함되므로 코드와 함께 버전 관리됩니다.

```bash
bulti prompt show    # 최종 조립 결과를 그대로 출력합니다 (디버깅·검증)
bulti prompt edit    # 글로벌 파일을 $EDITOR로 엽니다
```

## 네이티브 도구

도구의 정의와 실행은 모두 `src/tools/`에 있습니다. MCP·스킬 도구를 제외한 네이티브
도구 스키마는 정의가 짧기 때문에 기본으로 요청에 포함합니다.

| 도구 | 인자 | 설계 포인트 |
|------|------|-------------|
| `bash` | `command`, `timeout?` | cwd를 프로젝트 루트로 고정하며 셸 상태를 유지하지 않습니다. 출력은 64KB 상한을 룬 경계에서 절단합니다 |
| `read_file` | `path`, `offset?`, `limit?` | 기본 200줄 창이며, 페이징 푸터에 다음 offset을 명시합니다. auto-advance를 지원합니다 |
| `write_file` | `path`, `content` | 부모 디렉터리를 자동 생성하며, 빈 content도 명시적 생성으로 취급합니다 |
| `edit_file` | `path`, `find`, `replace`, `replace_all?` | 정확 문자열 치환입니다. 다중 발견 시 `replace_all`이 아니면 오류입니다 |
| `glob` | `pattern` | `.git`을 무시하며, 결과 상한과 패턴 좁히기 힌트를 제공합니다 |
| `grep` | `pattern`, `glob?`, `path?` | 자체 구현(walkdir + regex)이며, 결과 상한과 힌트를 제공합니다 |
| `history_list` | `query?`, `limit?` | 실행 히스토리를 조회합니다 |
| `history_read` | `run_id` | 실행 하나를 전문으로 읽습니다 |
| `skill_load` | `name` | 스킬 본문을 로드합니다 |
| `mcp_tools` | `server` | 서버의 도구 목록을 반환합니다(옵트인 주입) |
| `mcp_call` | `server`, `tool`, `args(object)` | MCP 도구를 호출합니다 |

`read_file`은 각 줄에 줄 번호를 붙여 반환하므로, 모델이 `edit_file`의 `find`에서 정확한
줄을 참조할 수 있습니다. 비전 엔드포인트에서는 이미지 파일을 base64 `image_url`
content로 반환합니다.

도구 결과는 저장 직전에 8,000자에서 절단하며, 절단 메시지에 **도구별 행동 가능 힌트**
(파일로 리다이렉트한 뒤 페이징하거나 grep 패턴을 좁히라는 안내 등)를 붙입니다. 그래야
모델이 막다른 호출을 반복하지 않고 전략을 바꿉니다.

## 가드 체계

로컬 모델에는 잘 알려진 실패 양상이 있습니다. 불티는 `src/agent/guards.rs`에 테이블
테스트 기반 방어층을 두며, 모든 가드는 양성(잡아야 할 것)과 음성(잡으면 안 되는 것)
케이스를 함께 가집니다.

| 가드 | 트리거 | 동작 |
|------|--------|------|
| toolcall index 누적 | 후속 청크에 id/name 없음 | index 키로 arguments 누적 |
| `required:null → []` | 스키마 직렬화 | 빈 배열로 정규화 |
| 한글 토큰 추정 보정 | 룬 기반 추정 | ASCII 4:1, 비ASCII 1:1 |
| 빈 응답 루프 | content가 빈 턴 연속 | 6턴에서 incomplete |
| 스트림 반복 감지 | 마지막 ~4KB에서 동일 라인 8회 또는 짧은 문구 8회 | 스트림 즉시 중단 → "repetition" incomplete |
| stuck 도구 시그니처 | 동일 (도구+인자) 시그니처 4턴 연속 | incomplete "no progress" |
| U+FFFD 퇴행 | U+FFFD 비율 ≥ 0.2 (최소 20 룬) | incomplete "silent context overflow" |
| future-intention nudge | 도구 호출 0 + "~하겠습니다" 문장 종결 | 완료 대신 nudge (최대 2회) |
| build gate | 코드 수정 후 최종 메시지가 빌드를 언급했는데 `bash`를 호출하지 않음 | incomplete "build verification never run" |
| pause-summary | "중단 시점 / 다음 세션 / to be continued" 패턴 | nudge 2회 후 핸드오프로 라우팅 |
| 핸드오프 품질 게이트 | 요약 길이·필수 섹션·degenerate | trim 폴백 |
| 핸드오프 depth | depth ≥ 12 | 이후 핸드오프 금지, incomplete |

## 설정

설정은 `~/.bulti/config.toml`(권장 권한 600)에 있습니다. 전체 레퍼런스는
[`DESIGN.md`](DESIGN.md) §3.1에 있습니다.

```toml
version = 1
active_endpoint = "main"
language = "en"                 # en | ko | ja

[endpoints.main]
url = "http://127.0.0.1:8084/v1"
api_key = "..."                 # 생략 가능 (로컬 무키 서버)
model = "qwen3.8-27b-q2"
context_tokens = 0              # 0이면 자동 프로브
vision = true                   # 비전 가능 모델 토글
thinking = true                 # reasoning_content 표시·기록 여부
max_iterations = 200            # 세그먼트당 도구 호출 턴 상한
# reasoning_effort = "medium"   # low | medium | high
# input_price_per_mtok = 0.0    # 선택적 비용 표시
# output_price_per_mtok = 0.0

[mcp.files]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/home/me"]
description = "파일시스템 접근"

[context]
handoff_threshold_pct = 75      # 추정 토큰이 ctx의 이 비율을 넘으면 핸드오프 트리거
max_handoff_depth = 12          # 체인 깊이 상한 (런어웨이 가드)
handoff_warn_depth = 8          # 경고 시작 깊이

[update]
repo = "agurrrrr/bulti"
mode = "check"                  # check | download | off
```

```bash
bulti config get <key>
bulti config set <key> <value>
bulti config list
```

전체 설정 디렉터리 배치는 다음과 같습니다.

```
~/.bulti/
├── config.toml          # 전체 설정 (권장 권한 600)
├── history.db           # 작업 히스토리 (SQLite)
├── update.json          # 릴리즈 체크 캐시 (etag, 확인 시각)
├── prompts/
│   └── default.md       # 글로벌 시스템 프롬프트 추가 지시 (선택)
├── sessions/            # 대화형 세션
│   └── <session_id>.json
└── skills/
    ├── <name>.md        # 단일 파일 스킬
    └── <name>/SKILL.md  # 리소스 디렉터리를 동반하는 스킬

<프로젝트 루트>/
└── .bulti/
    ├── system.md        # 프로젝트 시스템 프롬프트 추가 지시 (선택)
    └── skills/          # 프로젝트 스킬 (글로벌과 동명이면 프로젝트가 우선)
```

## 자동 업데이트

- **엔드포인트:** `GET https://api.github.com/repos/{repo}/releases/latest` (repo는
  `[update] repo`에서 가져오며, 빌드 시 `BULTI_REPO` 환경 변수로 주입되고 기본값은
  `agurrrrr/bulti`입니다).
- **확인 주기:** 결과(etag와 시각)를 `~/.bulti/update.json`에 캐시하고 최대 24시간마다만
  재확인합니다. run 시작 시 백그라운드 태스크가 stderr에 한 줄 알림을 출력합니다.
- **`bulti update`:** semver 태그를 비교하고, 실행 중인 대상 트리플에 맞는 아티팩트를
  찾고(예: `bulti-x86_64-unknown-linux-musl.tar.gz`), `checksums.txt`가 있으면 sha256을
  검증하고, 임시 디렉터리에 해제한 뒤 종료 시점에 실행 파일을 교체합니다
  (`self_replace`). 따라서 실행 중인 프로세스가 자기 자신을 교체당하지 않습니다.
- **모드:** `check`(기본, 알림만), `download`(확인 후 자동 다운로드·교체), `off`.
  `bulti update --check`는 확인만 하고 아무것도 바꾸지 않습니다.

## 아키텍처

불티는 명확한 모듈 경계를 가진 단일 크레이트입니다. 규모가 커지면 워크스페이스로
분리할 수 있도록 구성했습니다.

```
bulti/
├── Cargo.toml
├── DESIGN.md                # 전체 설계 문서
├── README.md / README_KO.md
├── LICENSE                  # MIT
├── examples/                # 오케스트레이션 예제 (셸, CI, 서브프로세스)
├── tests/                   # 통합/e2e 테스트 (wiremock SSE)
├── .github/workflows/       # CI (fmt, clippy -D warnings, test, release build)
└── src/
    ├── main.rs              # 진입점, clap 파싱, exit code 매핑
    ├── lib.rs               # 라이브러리 크레이트 루트
    ├── cli/                 # 서브커맨드 (chat, run, endpoint, history, skill, mcp, prompt, config, update, version)
    ├── config.rs            # ~/.bulti/config.toml 로드·저장 (serde + toml)
    ├── endpoint/            # 엔드포인트 등록·프로브·컨텍스트 길이 확정
    ├── llm/                 # OpenAI 호환 클라이언트 (SSE 스트리밍, 툴콜 누적)
    ├── agent/
    │   ├── mod.rs           # run·session 오케스트레이션, 체인·세그먼트 관리
    │   ├── loop_.rs         # 세그먼트 루프, 완료 판정
    │   ├── context.rs       # 토큰 추정, 트리밍, 툴 결과 절단
    │   ├── handoff.rs       # 핸드오프 지시문·파서·품질 게이트
    │   └── guards.rs        # 퇴행·거짓 완료·stuck 가드
    ├── tools/               # 네이티브 툴 + ToolRegistry (정의·실행 통합)
    ├── session/             # 대화형 세션 저장·로드·삭제
    ├── history/             # rusqlite 저장·조회
    ├── skills/              # 레이지 스킬 발견·로딩 (번들 예제)
    ├── mcp/                 # 레이지 MCP 클라이언트 (rmcp)
    ├── prompt/              # 시스템 프롬프트 계층 조립 (base.md)
    ├── slash/               # 슬래시 커맨드 레지스트리와 자동완성
    ├── completion/          # 프롬프트 히스토리 자동완성 소스
    ├── render/              # Markdown → ANSI/TUI 렌더링
    ├── i18n/                # en/ko/ja 카탈로그와 언어 상태
    ├── tui/                 # 대화형 TUI 렌더링 (ratatui)
    └── update/              # GitHub 릴리즈 확인과 self-replace
```

기술 스택은 `tokio`(rt-multi-thread, macros, process, fs, io-util), rustls를 사용하는
`reqwest` 0.12, `eventsource-stream`, `serde`/`serde_json`/`toml`, `clap` v4 derive,
`rusqlite`(번들), `dirs`, `walkdir`/`glob`/`regex`, `rmcp`, `ratatui`/`crossterm`,
`semver`/`sha2`/`tar`/`flate2`/`tempfile`, `thiserror`/`anyhow`/`tracing`입니다.

## 개발

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

CI 워크플로([`.github/workflows/ci.yml`](.github/workflows/ci.yml))는 `main` 브랜치
푸시와 모든 풀 리퀘스트에서 위 검사를 그대로 실행합니다. 통합 테스트는 `wiremock`으로
SSE 엔드포인트를 흉내 내므로 실제 모델 서버가 필요하지 않습니다.

**빌드 게이트:** 코드 변경은 `cargo build`와 `cargo test`가 통과하기 전에는 완료로
선언하지 않습니다. 에이전트 자체의 build gate가 런타임에서도 같은 규칙을 강제합니다.

## 기여

이슈와 풀 리퀘스트를 환영합니다. PR을 열기 전에 위 검사를 실행해 주시고, 큰 기능은
방향을 맞추기 위해 먼저 이슈를 열어 논의해 주십시오. 표면적을 작게 유지하는 것이 이
프로젝트의 명시적 목표입니다. 설계 결정은 [`DESIGN.md`](DESIGN.md)에 기록합니다.

## 라이선스

MIT License — [LICENSE](LICENSE)를 참고하십시오.

---

> ***"불은 제단 위에서 항상 타오르게 하여 꺼지지 않게 하라."***
> — 레위기 6:13
