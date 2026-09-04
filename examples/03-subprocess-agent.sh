#!/usr/bin/env bash
# 예제 3: 다른 에이전트/도구에서 bulti run 을 subprocess 로 호출
#
# - stdin(`-`) + --json 으로 기계가 읽기 쉬운 결과를 얻는다.
# - exit code 규약으로 성공/실패/미완료를 구분한다.
# - 파괴 명령 없이 자율 실행되므로 승인 프롬프트가 없다.
set -euo pipefail

# 호출자(상위 에이전트)가 전달한 프롬프트.
TASK="$1"

# JSON 보고서를 파싱해 상위 에이전트가 사용할 요약만 추출.
REPORT_JSON=$(printf '%s' "$TASK" | bulti run - --json --quiet)
RC=$?

# exit code 규약:
#   0 = completed → 결과 텍스트를 반환
#   1 = failed     → 오류로 보고
#   2 = incomplete → 상위 에이전트가 프롬프트를 조정해 재시도 권고
#   130 = interrupted → 중단으로 보고
case "$RC" in
  0)
    # 최종 결과 텍스트 추출.
    RESULT=$(printf '%s' "$REPORT_JSON" | jq -r '.result')
    echo "SUCCESS: $RESULT"
    ;;
  1)
    echo "FAILED: 체인 실행 실패" >&2
    exit 1
    ;;
  2)
    echo "INCOMPLETE: depth/시간 가드 도달 — 프롬프트를 좁혀 재시도" >&2
    exit 2
    ;;
  130)
    echo "INTERRUPTED: SIGINT 수신" >&2
    exit 130
    ;;
esac