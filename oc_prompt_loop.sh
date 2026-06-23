#!/bin/bash
while true; do
  output=$(opencode -s ses_12dc0300bffe6YHSpILR58QPVL run --thinking true --dangerously-skip-permissions "$(cat PROMPT.md)")
  echo "$output"
  if [ "$(echo "$output" | tr -d '[:space:]')" = "done" ]; then
    break
  fi
done
