#!/usr/bin/env bash
# 예제 2: CI 잡에서 bulti run 사용 (GitLab CI / GitHub Actions 공통 패턴)
#
# - exit code 규약에 따라 잡 성공/실패 판단
# - --json 보고서를 아티팩트로 저장
# - --max-time 으로 CI 타임아웃보다 짧게 상한 설정
set -euo pipefail

# CI 환경에서 TTY가 없으므로 진행 출력은 최소화된다.
# --json: stdout이 보고서 1회, 진행 출력은 전부 stderr.
REPORT="bulti-report.json"

# exit code 규약에 따른 잡 상태 매핑:
#   0 → 성공, 1 → 실패, 2 → 미완료(재시도 가능), 130 → 타임아웃/중단
bulti run "$CI_PROMPT" --json --max-time 300 \
  > "$REPORT" 2> bulti-progress.log
RC=$?

# 보고서를 아티팩트로 보존.
cat "$REPORT"

# status 필드로 상세 판단.
STATUS=$(jq -r '.status' "$REPORT")
echo "체인 상태: $STATUS (exit $RC)"

case "$RC" in
  0) echo "잡 성공" ;;
  1) echo "잡 실패: 엔드포인트 오류 또는 치명 버그" >&2; exit 1 ;;
  2) echo "잡 미완료: 재시도 가능" >&2; exit 2 ;;
  130) echo "잡 중단: SIGINT/타임아웃" >&2; exit 130 ;;
esac