#!/bin/sh
# Bazel's workspace status (--workspace_status_command, see .bazelrc): the
# commit, which stamped builds (--stamp) put in the ninja-rs version.
# Nothing outside a git checkout.
commit=$(git rev-parse --short=12 HEAD 2>/dev/null) && echo "STABLE_GIT_COMMIT $commit"
exit 0
