import argparse, json, os, pathlib, subprocess, sys, time
p = argparse.ArgumentParser()
p.add_argument("provider")
p.add_argument("prompt")
p.add_argument("output")
p.add_argument("--timeout", type=int, default=240)
a = p.parse_args()
root = pathlib.Path(__file__).resolve().parents[4]
prompt = pathlib.Path(a.prompt).read_text(encoding="utf-8-sig")
output = pathlib.Path(a.output).resolve()
log = output.with_suffix(".log")
env = os.environ.copy()
env["MNEMO_DISABLE"] = "1"
env["PYTHONDONTWRITEBYTECODE"] = "1"
if a.provider == "codex":
    cli = pathlib.Path(os.environ["APPDATA"]) / "npm/node_modules/@openai/codex/bin/codex.js"
    command = ["node", str(cli), "-a", "never", "exec", "--sandbox", "read-only", "--skip-git-repo-check", "--output-last-message", str(output), "-"]
    stdin = prompt
else:
    cli = pathlib.Path(os.environ["LOCALAPPDATA"]) / "agy/bin/agy.exe"
    command = [str(cli), "-p", prompt, "--output-format", "text"]
    stdin = None
started = time.monotonic()
try:
    r = subprocess.run(command, input=stdin, text=True, encoding="utf-8", errors="replace", capture_output=True, cwd=root, env=env, timeout=a.timeout, creationflags=getattr(subprocess,"CREATE_NO_WINDOW",0))
    log.write_text(r.stderr + "\nEXIT=" + str(r.returncode),encoding="utf-8")
    if a.provider != "codex" and r.returncode == 0:
        output.write_text(r.stdout,encoding="utf-8")
    elif a.provider == "codex":
        log.write_text(r.stdout + "\n" + r.stderr + "\nEXIT=" + str(r.returncode),encoding="utf-8")
    ok = r.returncode == 0 and output.exists() and output.stat().st_size > 100
    print(json.dumps({"provider":a.provider,"ok":ok,"exit":r.returncode,"output":str(output),"elapsed_s":round(time.monotonic()-started,1)}))
except subprocess.TimeoutExpired as e:
    log.write_text("TIMEOUT after " + str(a.timeout) + " seconds", encoding="utf-8")
    print(json.dumps({"provider":a.provider,"ok":False,"timeout":a.timeout,"output":str(output)}))
    sys.exit(2)
