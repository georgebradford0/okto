import argparse
import os
import re
import subprocess
import sys
import argcomplete
from git import Repo, InvalidGitRepositoryError


def worktree_completer(prefix, parsed_args, **kwargs):
    try:
        repo = Repo(os.getcwd(), search_parent_directories=True)
        repo_name = os.path.basename(repo.working_dir)
        worktrees_dir = os.path.expanduser(f"~/.claudulhu/worktrees/{repo_name}")
        if not os.path.isdir(worktrees_dir):
            return []
        return [w for w in os.listdir(worktrees_dir) if w.startswith(prefix)]
    except Exception:
        return []


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
    return result.stdout.strip().lower().replace(" ", "-")


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
    completion_block = (
        "\n# claudulhu tab completion\n"
        "autoload -U bashcompinit\n"
        "bashcompinit\n"
        f'eval "$({os.path.join(os.path.dirname(sys.executable), "register-python-argcomplete").replace(os.path.expanduser("~"), "~")} claudulhu)"\n'
    )
    zshrc = os.path.expanduser("~/.zshrc")
    if os.path.isfile(zshrc):
        with open(zshrc) as f:
            if "-m argcomplete claudulhu" in f.read():
                print("Completions already installed.")
                return
    with open(zshrc, "a") as f:
        f.write(completion_block)
    print("Completions installed. Run 'source ~/.zshrc' to activate.")


def uninstall_completions() -> None:
    zshrc = os.path.expanduser("~/.zshrc")
    if not os.path.isfile(zshrc):
        print("No ~/.zshrc found.")
        return
    with open(zshrc) as f:
        contents = f.read()
    updated = re.sub(r"\n# claudulhu tab completion\nautoload -U bashcompinit\nbashcompinit\neval \"\$\(.*?claudulhu.*?\)\"\n", "", contents)
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

    task_parser = subparsers.add_parser("task", help="Run a task in a new worktree")
    task_parser.add_argument("description", help="Task description")

    subparsers.add_parser("list", help="List worktrees for the current repo")

    completions_parser = subparsers.add_parser("completions", help="Manage shell completions")
    completions_subparsers = completions_parser.add_subparsers(dest="completions_command")
    completions_subparsers.add_parser("install", help="Install tab completions into ~/.zshrc")
    completions_subparsers.add_parser("uninstall", help="Remove tab completions from ~/.zshrc")

    subparsers.add_parser("uninstall", help="Remove all claudulhu data and shell completions")

    merge_parser = subparsers.add_parser("merge", help="Merge a worktree branch into the current branch")
    merge_parser.add_argument("name", help="Worktree name (branch) to merge").completer = worktree_completer

    remove_parser = subparsers.add_parser("remove", help="Remove a worktree and its branch")
    remove_parser.add_argument("name", help="Worktree name (branch) to remove").completer = worktree_completer

    argcomplete.autocomplete(parser)
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
    elif args.command == "task":
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


if __name__ == "__main__":
    main()
