#!/usr/bin/env python3
"""Start/stop the independent server supervision Web GUI."""
import sys
from app import main, Problem

if __name__ == "__main__":
    try:
        raise SystemExit(main(mode="server"))
    except (Problem, OSError) as error:
        print(f"webgui: {error}", file=sys.stderr)
        raise SystemExit(1)
