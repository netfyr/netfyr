#!/bin/bash
if [ -n "$(git diff HEAD --stat)" ]; then
  echo "=== Uncommitted changes ==="
  git diff HEAD --stat
  echo ""
  git diff HEAD
else
  echo "=== Last commit ==="
  git log -1 --oneline
  git diff HEAD~1 --stat
  echo ""
  git diff HEAD~1
fi
