import argparse
import os
import re
import subprocess
import sys
from git import Repo, InvalidGitRepositoryError

ZSH_COMPLETION_FUNC = r"""#compdef claudulhu

_claudulhu_worktrees() {
  local repo_root repo_name worktrees_dir
  repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || return
  repo_name="${repo_root:t}"
  worktrees_dir="${HOME}/.claudulhu/worktrees/${repo_name}"
  [[ -d "$worktrees_dir" ]] || return
  local -a worktrees
  worktrees=("${worktrees_dir}"/*(N:t))
  compadd -a worktrees
}

_claudulhu_task_desc() {
  # Work from $PREFIX (the space-split token at the cursor), not words[CURRENT].
  # IPREFIX already holds everything before this token (including any leading quote
  # and prior words), so we must not double-count it.
  [[ "$PREFIX" != *@* ]] && return
  local prefix_before_at="${PREFIX%@*}"
  local at_part="${PREFIX##*@}"
  local dir_part file_part
  if [[ "$at_part" == */* ]]; then
    dir_part="${at_part%/*}/"
    file_part="${at_part##*/}"
  else
    dir_part=""
    file_part="$at_part"
  fi
  local search="${dir_part:-.}"
  local -a entries completions
  entries=("${search%/}"/${file_part}*(N:t))
  (( $#entries )) || return
  local e
  for e in $entries; do
    [[ -d "${dir_part}${e}" ]] && completions+=("${e}/") || completions+=("${e}")
  done
  IPREFIX="${IPREFIX}${prefix_before_at}@${dir_part}"
  PREFIX="$file_part"
  compadd -Q -S '' -a completions
}

_claudulhu() {
  local subcmd="${words[2]}"

  if (( CURRENT == 2 )); then
    local -a commands=(
      'branch:Run a task in a new worktree'
      'list:List worktrees for the current repo'
      'completions:Manage shell completions'
      'uninstall:Remove all claudulhu data and shell completions'
      'merge:Merge a worktree branch into the current branch'
      'remove:Remove a worktree and its branch'
    )
    _describe 'command' commands
    return
  fi

  case $subcmd in
    branch)
      _claudulhu_task_desc
      ;;
    merge|remove)
      _claudulhu_worktrees
      ;;
    completions)
      if (( CURRENT == 3 )); then
        local -a subcommands=(
          'install:Install tab completions into ~/.zshrc'
          'uninstall:Remove tab completions from ~/.zshrc'
        )
        _describe 'subcommand' subcommands
      fi
      ;;
  esac
}

_claudulhu "$@"
"""

ZSH_FUNCTIONS_DIR = os.path.expanduser("~/.zsh_functions")
ZSH_COMPLETION_FILE = os.path.join(ZSH_FUNCTIONS_DIR, "_claudulhu")
ZSHRC_BLOCK = (
    "\n# claudulhu tab completion\n"
    "(( $+functions[compdef] )) || { autoload -Uz compinit && compinit -u; }\n"
    "unfunction _claudulhu _claudulhu_task_desc _claudulhu_worktrees 2>/dev/null\n"
    "autoload -Uz _claudulhu\n"
    "compdef _claudulhu claudulhu\n"
)


def generate_branch_name(task: str, taken: list[str] | None = None) -> str:
    prompt = (
        f"Generate a short, lowercase, hyphenated git branch name (2-5 words, no punctuation) "
        f"for this task: {task}. Reply with only the branch name, nothing else."
    )
    if taken:
        prompt += f" Do not use any of these names as they are already taken: {', '.join(taken)}."
    result = subprocess.run(
        ["claude", "-p", prompt],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        print(f"Error generating branch name: {result.stderr}", file=sys.stderr)
        sys.exit(1)
    name = result.stdout.strip().lower().replace(" ", "-")
    return re.sub(r"[^a-z0-9-]", "", name)


def create_worktree(repo: Repo, branch: str) -> str:
    repo_name = os.path.basename(repo.working_dir)
    worktrees_dir = os.path.expanduser(f"~/.claudulhu/worktrees/{repo_name}")
    os.makedirs(worktrees_dir, exist_ok=True)
    worktree_path = os.path.join(worktrees_dir, branch)
    repo.git.worktree("add", "-b", branch, worktree_path)
    return worktree_path


def remove_worktree(repo: Repo, name: str) -> None:
    repo_name = os.path.basename(repo.working_dir)
    worktree_path = os.path.expanduser(f"~/.claudulhu/worktrees/{repo_name}/{name}")
    if not os.path.isdir(worktree_path):
        print(f"No worktree found for '{name}'.", file=sys.stderr)
        sys.exit(1)
    repo.git.worktree("remove", "--force", worktree_path)
    try:
        repo.delete_head(name, force=True)
        print(f"Removed worktree and branch '{name}'.")
    except Exception:
        print(f"Removed worktree '{name}' (branch could not be deleted).")


def install_completions() -> None:
    zshrc = os.path.expanduser("~/.zshrc")
    if os.path.isfile(zshrc):
        with open(zshrc) as f:
            if "# claudulhu tab completion" in f.read():
                print("Completions already installed.")
                return
    os.makedirs(ZSH_FUNCTIONS_DIR, exist_ok=True)
    with open(ZSH_COMPLETION_FILE, "w") as f:
        f.write(ZSH_COMPLETION_FUNC)
    with open(zshrc, "a") as f:
        f.write(ZSHRC_BLOCK)
    print("Completions installed. Run 'source ~/.zshrc' to activate.")


def uninstall_completions() -> None:
    if os.path.isfile(ZSH_COMPLETION_FILE):
        os.remove(ZSH_COMPLETION_FILE)
    zshrc = os.path.expanduser("~/.zshrc")
    if not os.path.isfile(zshrc):
        print("No ~/.zshrc found.")
        return
    with open(zshrc) as f:
        contents = f.read()
    updated = re.sub(
        r"\n# claudulhu tab completion\n(?:[^\n]*\n){1,4}",
        "",
        contents,
    )
    if updated == contents:
        print("Completions not found in ~/.zshrc.")
        return
    with open(zshrc, "w") as f:
        f.write(updated)
    print("Completions removed. Run 'source ~/.zshrc' to deactivate.")


def uninstall() -> None:
    confirm = input("This will remove ~/.claudulhu and shell completions. Continue? [y/N] ")
    if confirm.strip().lower() != "y":
        print("Aborted.")
        return

    import shutil
    claudulhu_dir = os.path.expanduser("~/.claudulhu")
    if os.path.isdir(claudulhu_dir):
        shutil.rmtree(claudulhu_dir)
        print(f"Removed {claudulhu_dir}")
    else:
        print("No ~/.claudulhu directory found.")

    uninstall_completions()

    print("Run 'uv tool uninstall claudulhu' to remove the binary.")


def merge_worktree(repo: Repo, name: str) -> None:
    existing = [h.name for h in repo.heads]
    if name not in existing:
        print(f"No branch '{name}' found.", file=sys.stderr)
        sys.exit(1)
    try:
        repo.git.merge(name)
        print(f"Merged '{name}' into '{repo.active_branch.name}'.")
    except Exception as e:
        print(f"Merge failed: {e}", file=sys.stderr)
        sys.exit(1)


def list_worktrees(repo: Repo) -> None:
    repo_name = os.path.basename(repo.working_dir)
    worktrees_dir = os.path.expanduser(f"~/.claudulhu/worktrees/{repo_name}")
    if not os.path.isdir(worktrees_dir):
        print("No worktrees found.")
        return
    worktrees = sorted(os.listdir(worktrees_dir))
    if not worktrees:
        print("No worktrees found.")
        return
    for name in worktrees:
        path = os.path.join(worktrees_dir, name)
        print(f"{name}  ({path})")


def main():
    parser = argparse.ArgumentParser(prog="claudulhu")
    subparsers = parser.add_subparsers(dest="command")

    task_parser = subparsers.add_parser("branch", help="Run a task in a new worktree")
    task_parser.add_argument("description", help="Task description (use double quotes)")

    subparsers.add_parser("list", help="List worktrees for the current repo")

    completions_parser = subparsers.add_parser("completions", help="Manage shell completions")
    completions_subparsers = completions_parser.add_subparsers(dest="completions_command")
    completions_subparsers.add_parser("install", help="Install tab completions into ~/.zshrc")
    completions_subparsers.add_parser("uninstall", help="Remove tab completions from ~/.zshrc")

    subparsers.add_parser("uninstall", help="Remove all claudulhu data and shell completions")

    merge_parser = subparsers.add_parser("merge", help="Merge a worktree branch into the current branch")
    merge_parser.add_argument("name", help="Worktree name (branch) to merge")

    remove_parser = subparsers.add_parser("remove", help="Remove a worktree and its branch")
    remove_parser.add_argument("name", help="Worktree name (branch) to remove")

    args = parser.parse_args()
    if args.command == "uninstall":
        uninstall()
    elif args.command == "completions":
        if args.completions_command == "install":
            install_completions()
        elif args.completions_command == "uninstall":
            uninstall_completions()
        else:
            completions_parser.print_help()
    elif args.command == "merge":
        try:
            repo = Repo(os.getcwd(), search_parent_directories=True)
        except InvalidGitRepositoryError:
            print("No git repository found in current directory.", file=sys.stderr)
            sys.exit(1)
        merge_worktree(repo, args.name)
    elif args.command == "remove":
        try:
            repo = Repo(os.getcwd(), search_parent_directories=True)
        except InvalidGitRepositoryError:
            print("No git repository found in current directory.", file=sys.stderr)
            sys.exit(1)
        remove_worktree(repo, args.name)
    elif args.command == "list":
        try:
            repo = Repo(os.getcwd(), search_parent_directories=True)
        except InvalidGitRepositoryError:
            print("No git repository found in current directory.", file=sys.stderr)
            sys.exit(1)
        list_worktrees(repo)
    elif args.command == "branch":
        try:
            repo = Repo(os.getcwd(), search_parent_directories=True)
        except InvalidGitRepositoryError:
            print("No git repository found in current directory.", file=sys.stderr)
            sys.exit(1)

        print("Generating branch name...")
        max_attempts = 5
        taken = []
        for attempt in range(1, max_attempts + 1):
            branch = generate_branch_name(args.description, taken or None)
            print(f"Branch:   {branch}")
            existing = [h.name for h in repo.heads]
            if branch not in existing:
                break
            taken.append(branch)
            print(f"  '{branch}' is already taken, retrying... ({attempt}/{max_attempts})")
        else:
            print("Error: could not generate a unique branch name after 5 attempts.", file=sys.stderr)
            sys.exit(1)

        print("Creating worktree...")
        worktree_path = create_worktree(repo, branch)
        print(f"Worktree: {worktree_path}")

        print(f"Starting Claude Code session...")
        os.chdir(worktree_path)
        os.execvp("claude", ["claude", "--dangerously-skip-permissions", args.description])
    else:
        parser.print_help()


if __name__ == "__main__":
    main()
