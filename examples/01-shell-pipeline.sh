#!/usr/bin/env bash
# 예제 1: 셸 파이프라인에서 bulti run 사용
#
# - stdin(`-`)으로 프롬프트를 파이프로 전달
# - `--json`으로 stdout을 기계 전용 보고서로 만들고, 진행 출력은 stderr
# - exit code 규약: 0=completed, 1=failed, 2=incomplete, 130=interrupted
#
# 사용법: ./examples/01-shell-pipeline.sh "오늘 할 일을 요약해줘"
set -euo pipefail

PROMPT="${1:-$(cat <<'EOF'
프로젝트의 README.md 파일을 읽고 핵심 기능 3가지를 요약해줘.
EOF
)}"

# stdin 파이프 + --json 보고서 → stdout으로 보고서만 받는다.
# jq로 status 필드를 추출해 분기 처리한다.
echo "$PROMPT" | bulti run - --json --quiet \
  | jq -e '.status == "completed"' \
  || { echo "체인 미완료/실패" >&2; exit 2; }

# 보고서의 핵심 필드를 출력.
echo "$PROMPT" | bulti run - --json --quiet \
  | jq '{status, handoff_depth, segments, files_touched, input_tokens, output_tokens}'

echo "체인 완료 (exit 0)" >&2