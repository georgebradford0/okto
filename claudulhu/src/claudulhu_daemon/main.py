"""claudulhu-daemon entry point."""

from __future__ import annotations

import argparse
import os
import sys

from git import InvalidGitRepositoryError, Repo


def main() -> None:
    parser = argparse.ArgumentParser(
        prog="claudulhud",
        description="Git repo management server — WebSocket chat + worker REST API",
    )
    parser.add_argument(
        "--repo",
        metavar="PATH",
        help="Git repo to manage (default: current directory)",
    )
    parser.add_argument(
        "--host",
        default="0.0.0.0",
        metavar="HOST",
        help="Bind host (default: 0.0.0.0)",
    )
    parser.add_argument(
        "--port",
        type=int,
        default=8000,
        metavar="PORT",
        help="Bind port (default: 8000)",
    )
    parser.add_argument(
        "--model",
        default="claude-opus-4-6",
        metavar="MODEL",
        help="Claude model for chat sessions and workers (default: claude-opus-4-6)",
    )
    parser.add_argument(
        "--poll-interval",
        type=float,
        default=10.0,
        metavar="SECONDS",
        help="Git poll interval in seconds (default: 10)",
    )
    args = parser.parse_args()

    # Resolve repo
    repo_path = args.repo or os.getcwd()
    try:
        repo = Repo(repo_path, search_parent_directories=True)
    except InvalidGitRepositoryError:
        print(f"error: not a git repository: {repo_path}", file=sys.stderr)
        sys.exit(1)

    resolved = repo.working_dir
    print(f"[claudulhud] repo   : {resolved}")
    print(f"[claudulhud] model  : {args.model}")
    print(f"[claudulhud] listen : {args.host}:{args.port}")

    # Wire app state before uvicorn imports the app module
    from .app import app, state
    from .monitor import GitMonitor
    from .sessions import SessionStore
    from .workers import WorkerPool

    state.repo = repo
    state.repo_path = resolved
    state.model = args.model
    state.monitor = GitMonitor(repo, poll_interval=args.poll_interval)
    state.workers = WorkerPool()
    state.sessions = SessionStore(repo_name=os.path.basename(resolved))

    import uvicorn
    uvicorn.run(app, host=args.host, port=args.port)


if __name__ == "__main__":
    main()
