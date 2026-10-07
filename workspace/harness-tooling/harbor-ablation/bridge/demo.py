"""Create an original synthetic offline package in one new owned output directory."""
import argparse
import json
import subprocess
from pathlib import Path
import format_bridge
import synthetic


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exporter", required=True)
    parser.add_argument("--output-dir", required=True)
    args = parser.parse_args()
    output = Path(args.output_dir).absolute()
    if output.parent.resolve() != output.parent:
        raise ValueError("output_parent_not_canonical")
    output.mkdir(mode=0o700)
    source = synthetic.fixture(output / "inputs")
    common = ["--corpus", str(source/"corpus.json"), "--readers", str(source/"readers.json"),
              "--options", str(source/"options.json")]
    exporter = str(Path(args.exporter).resolve())
    for command, destination in (("plan", "plan"), ("export-readers", "readers")):
        subprocess.run([exporter, command, *common, "--output-dir", str(output/destination)], check=True, timeout=30)
    grader = synthetic.grader_projection(source, output/"readers")
    subprocess.run([exporter, "export-verifier", "--graders", str(grader), "--reader-export", str(output/"readers"),
                    "--output-dir", str(output/"graders")], check=True, timeout=30)
    for destination in ("plan", "readers", "graders"):
        subprocess.run([exporter,"validate","--export-dir",str(output/destination)],check=True,timeout=30)
    print(json.dumps(format_bridge.compose(output/"readers",output/"graders",output/"harbor"),sort_keys=True))


if __name__ == "__main__":
    main()
