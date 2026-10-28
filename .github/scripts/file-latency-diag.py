"""Diagnostic matrix. All variants use one production builder and identical release flags."""
import difflib
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

ROOT = Path.cwd()
HERE = ROOT / "benchmarks/http-servers"
OUT = Path(os.environ["BENCHMARK_OUTPUT"])
WORK = Path(os.environ["RUNNER_TEMP"]) / "file-latency-builds"
BASE = "f7fc25c585983364b583ef67308a2a5c9715f0ef"
assert subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip() == BASE
OUT.mkdir(parents=True, exist_ok=True)
WORK.mkdir(parents=True, exist_ok=True)
RANGES = "crates/gatel-bundle-std/src/ranges.rs"
BODY = "crates/gatel-bundle-std/src/file_body.rs"
original = {p:(ROOT/p).read_text() for p in (RANGES,BODY)}
SCENARIOS = ["static-4k","static-16k","static-32k","static-63k","static-64k","static-64k-plus1","static-65k","static-1m","static-10m"]

def run(args, **kwargs):
    print("$", " ".join(map(str,args)), flush=True)
    return subprocess.run(list(map(str,args)),check=True,**kwargs)

def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()

def variants():
    exact = original[BODY]
    start = exact.index("pub(crate) fn read_exact_prefix(")
    end = exact.index("pub(crate) struct ExactReader", start)
    exact = exact[:start] + """pub(crate) fn read_exact_prefix(mut reader: impl Read, length: usize) -> io::Result<Vec<u8>> {
    let mut buffer = vec![0; length];
    reader.read_exact(&mut buffer)?;
    Ok(buffer)
}

""" + exact[end:]
    split = original[RANGES].replace(
        "response.body(ResBody::Once(buffer.into()));",
        """let bytes = bytes::Bytes::from(buffer);
        if bytes.len() > 8192 {
            let middle = bytes.len() / 2;
            response.body(ResBody::Chunks(std::collections::VecDeque::from([
                bytes.slice(..middle), bytes.slice(middle..),
            ])));
        } else {
            response.body(ResBody::Once(bytes));
        }""",1)
    stream = original[RANGES].replace("const INLINE_LIMIT: u64 = 64 * 1024;", "const INLINE_LIMIT: u64 = 8 * 1024;").replace(
        "const CHUNK: usize = INLINE_LIMIT as usize;", "const CHUNK: usize = 64 * 1024;")
    return {
        "base":{},
        "exact":{BODY:exact},
        "split":{RANGES:split},
        "stream8":{RANGES:stream},
        "chunk32":{RANGES:original[RANGES].replace("const CHUNK: usize = INLINE_LIMIT as usize;","const CHUNK: usize = 32 * 1024;")},
        "chunk128":{RANGES:original[RANGES].replace("const CHUNK: usize = INLINE_LIMIT as usize;","const CHUNK: usize = 128 * 1024;")},
    }

def build():
    manifest = {
        "base_revision":BASE,"execution_revision":os.environ["GITHUB_SHA"],
        "compiler":subprocess.check_output(["docker","run","--rm","--entrypoint","rustc","gatel-latency-builder:base","-Vv"],text=True),
        "cargo_command":"cargo build --release --locked -p gatel",
        "dockerfile_sha256":sha(ROOT/"Dockerfile"), "lock_sha256":sha(ROOT/"Cargo.lock"),
        "note":"Every build reuses only base compiled dependencies. Changed Rust files are copied and touched before the unchanged release command. All runtime images use the unmodified production Dockerfile SOURCE=prebuilt.",
        "variants":{},
    }
    for name, changes in variants().items():
        directory = WORK/name
        directory.mkdir(exist_ok=True)
        target = directory/"context/linux/amd64/gatel"
        target.parent.mkdir(parents=True,exist_ok=True)
        shutil.copyfile(ROOT/"Dockerfile",directory/"Dockerfile")
        container = "gatel-latency-build-"+name
        touch = " ".join("/src/"+p for p in changes)
        command = ("touch "+touch+" && " if touch else "") + "cargo build --release --locked -p gatel && cp target/release/gatel /out/gatel"
        if not changes:
            command = "true"
        run(["docker","create","--name",container,"--workdir","/src","--entrypoint","sh","gatel-latency-builder:base","-c",command])
        try:
            patches=[]
            for path, content in changes.items():
                local = OUT/"patches"/name/path
                local.parent.mkdir(parents=True,exist_ok=True)
                local.write_text(content)
                run(["docker","cp",local,container+":/src/"+path])
                patches.extend(difflib.unified_diff(original[path].splitlines(True),content.splitlines(True),fromfile="a/"+path,tofile="b/"+path))
            if patches:
                (OUT/"patches"/(name+".patch")).write_text("".join(patches))
                run(["docker","start","-a",container])
                code=subprocess.check_output(["docker","inspect","--format","{{.State.ExitCode}}",container],text=True).strip()
                assert code=="0",(name,code)
            run(["docker","cp",container+":/out/gatel",target])
        finally:
            run(["docker","rm","-f",container])
        run(["docker","build","--build-arg","SOURCE=prebuilt","--build-arg","TARGETPLATFORM=linux/amd64","-t","gatel-latency:"+name,directory])
        manifest["variants"][name]={"binary_sha256":sha(target),"changed_sources":{p:hashlib.sha256(c.encode()).hexdigest() for p,c in changes.items()}}
        (OUT/"build-manifest.json").write_text(json.dumps(manifest,indent=2))
    images=run(["docker","image","inspect",*["gatel-latency:"+n for n in variants()]],capture_output=True,text=True)
    (OUT/"image-inspect.json").write_text(images.stdout)

def adapter():
    path=OUT/"diagnostic-harness.py"
    path.write_text("""import sys
sys.path.insert(0, """+repr(str(HERE))+""")
import run as bench
for name, size in [("16k",16384),("32k",32768),("63k",64512),("64k-plus1",65537)]:
    filename="payload-"+name+".bin"
    bench.PAYLOADS[filename]=size
    bench.SCENARIOS["static-"+name]={"description":"Diagnostic "+name,"service":"{target}-static","path":"/"+filename}
raise SystemExit(bench.main())
""")
    return path

def env_for(name):
    override=OUT/"overrides"/(name+".yml")
    override.parent.mkdir(exist_ok=True)
    override.write_text("services:\n  gatel-static:\n    image: gatel-latency:"+name+"\n  gatel-proxy:\n    image: gatel-latency:"+name+"\n")
    return dict(os.environ,COMPOSE_FILE=str(HERE/"compose.yml")+":"+str(override))

def measure():
    launcher=adapter()
    all_rows=[]
    for phase_id,name in enumerate(["base","exact","split","stream8","chunk32","chunk128","base"],1):
        phase=OUT/f"{phase_id:02d}-{name}"
        phase.mkdir()
        command=[sys.executable,"-B","-u",str(launcher),"--target","gatel","--no-build","--rounds","1","--duration","5","--warmup","2","--threads","4","--connections","128","--output-dir",str(phase)]
        for scenario in SCENARIOS: command.extend(["--scenario",scenario])
        print(f"PHASE {phase_id}/7: {name}",flush=True)
        with (phase/"run.log").open("w") as log:
            process=subprocess.Popen(command,env=env_for(name),stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
            for line in process.stdout:
                print(line,end="",flush=True);log.write(line);log.flush()
            if process.wait(): raise RuntimeError("diagnostic phase failed: "+name)
        rows=json.loads((phase/"summary.json").read_text())
        assert len(rows)==len(SCENARIOS)
        for row in rows: row.update(variant=name,phase=phase_id)
        all_rows.extend(rows)
        (OUT/"comparison.json").write_text(json.dumps(all_rows,indent=2))
    assert len(all_rows)==63
    print("Completed 63 exploratory measurements; these short runs select hypotheses, not final performance claims.",flush=True)

def trace():
    env=env_for("base")
    run(["docker","compose","up","-d","--no-build","bench","backend","gatel-static","gatel-proxy"],cwd=HERE,env=env)
    try:
        for attempt in range(60):
            if subprocess.run(["docker","compose","exec","-T","bench","curl","-fsS","-o","/dev/null","http://gatel-static:8080/payload-4k.txt"],cwd=HERE,env=env,capture_output=True).returncode==0: break
            time.sleep(1)
        container=subprocess.check_output(["docker","compose","ps","-q","gatel-static"],cwd=HERE,env=env,text=True).strip()
        pid=subprocess.check_output(["docker","inspect","--format","{{.State.Pid}}",container],text=True).strip()
        for scenario,file in [("static-64k","payload-64k.bin"),("static-65k","payload-65k.bin"),("static-10m","payload-10m.bin")]:
            with (OUT/(scenario+"-strace-stderr.txt")).open("w") as stderr:
                tracer=subprocess.Popen(["sudo","timeout","--signal=INT","8","strace","-f","-c","-p",pid,"-o",str(OUT/(scenario+"-syscalls.txt"))],stdout=subprocess.DEVNULL,stderr=stderr)
                time.sleep(.5)
                measured=run(["docker","compose","exec","-T","bench","wrk","-t","1","-c","1","-d","3s","--latency","-H","Accept-Encoding: identity","http://gatel-static:8080/"+file],cwd=HERE,env=env,capture_output=True,text=True)
                (OUT/(scenario+"-traced-wrk.txt")).write_text(measured.stdout)
                code=tracer.wait(timeout=15)
                print(f"strace {scenario}: exit {code}; tracing results are diagnostic only",flush=True)
    finally:
        run(["docker","compose","down","--volumes","--remove-orphans"],cwd=HERE,env=env)

{"build":build,"measure":measure,"trace":trace}[sys.argv[1]]()
