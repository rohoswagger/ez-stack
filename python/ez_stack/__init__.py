"""ez-stack: Stacked PRs for GitHub — agent-first CLI for parallel development."""

__version__ = "0.0.0"  # Overwritten by CI from Cargo.toml version


def main():
    """Entry point that runs the bundled ez binary."""
    import os
    import sys
    import subprocess

    # Find the binary bundled alongside this package.
    pkg_dir = os.path.dirname(os.path.abspath(__file__))
    bin_name = "ez.exe" if sys.platform == "win32" else "ez"
    bin_path = os.path.join(pkg_dir, "bin", bin_name)

    if not os.path.exists(bin_path):
        print(f"ez binary not found at {bin_path}", file=sys.stderr)
        print("This package bundles a platform-specific binary.", file=sys.stderr)
        print("Try: pip install --force-reinstall ez-stack", file=sys.stderr)
        sys.exit(1)

    # Make sure it's executable.
    os.chmod(bin_path, 0o755)

    # Exec the binary, passing through all args.
    try:
        result = subprocess.run([bin_path] + sys.argv[1:])
        sys.exit(result.returncode)
    except KeyboardInterrupt:
        sys.exit(130)
