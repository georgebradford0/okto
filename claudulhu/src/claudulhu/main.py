import argparse
import os
import subprocess
import sys
from git import Repo, InvalidGitRepositoryError


def generate_branch_name(task: str) -> str:
    prompt = (
        f"Generate a short, lowercase, hyphenated git branch name (2-5 words, no punctuation) "
        f"for this task: {task}. Reply with only the branch name, nothing else."
    )
    result = subprocess.run(
        ["claude", "-p", prompt],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        print(f"Error generating branch name: {result.stderr}", file=sys.stderr)
        sys.exit(1)
    return result.stdout.strip().lower().replace(" ", "-")


def create_worktree(repo: Repo, branch: str) -> str:
    repo_name = os.path.basename(repo.working_dir)
    worktrees_dir = os.path.expanduser(f"~/.claudulhu/worktrees/{repo_name}")
    os.makedirs(worktrees_dir, exist_ok=True)
    worktree_path = os.path.join(worktrees_dir, branch)
    repo.git.worktree("add", "-b", branch, worktree_path)
    return worktree_path


def main():
    parser = argparse.ArgumentParser(prog="claudulhu")
    subparsers = parser.add_subparsers(dest="command")

    task_parser = subparsers.add_parser("task", help="Run a task in a new worktree")
    task_parser.add_argument("description", help="Task description")

    args = parser.parse_args()
    if args.command == "task":
        try:
            repo = Repo(os.getcwd(), search_parent_directories=True)
        except InvalidGitRepositoryError:
            print("No git repository found in current directory.", file=sys.stderr)
            sys.exit(1)

        print("Generating branch name...")
        branch = generate_branch_name(args.description)
        print(f"Branch:   {branch}")

        print("Creating worktree...")
        worktree_path = create_worktree(repo, branch)
        print(f"Worktree: {worktree_path}")

        print(f"Starting Claude Code session...")
        os.chdir(worktree_path)
        os.execvp("claude", ["claude", "--dangerously-skip-permissions", args.description])


if __name__ == "__main__":
    main()
