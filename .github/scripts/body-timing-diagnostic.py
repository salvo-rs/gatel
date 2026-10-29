"""Screen transport fairness alternatives; production builds and archived baseline."""
import hashlib,io,json,os,re,statistics,subprocess,sys,threading,time,zipfile
from pathlib import Path
ROOT=Path.cwd();HERE=ROOT/"benchmarks/http-servers";OUT=Path(os.environ["BENCHMARK_OUTPUT"])
BASE="f11ffccdaaae6249c1f779160b5b84af1f479ae6"
VARIANTS={"timing":True}
ORDER=["production","timing","timing","production"]
SCENARIOS=["static-10m","static-20m"]

def run(args,**kw):
    print("$"," ".join(map(str,args)),flush=True)
    return subprocess.run(list(map(str,args)),check=True,**kw)


def digest(p): return hashlib.sha256(Path(p).read_bytes()).hexdigest()


def build():
    for attempt in range(3):
        try:
            payload=subprocess.check_output(["gh","api","repos/salvo-rs/gatel/actions/artifacts/12906571886/zip"],timeout=90)
            archived=zipfile.ZipFile(io.BytesIO(payload));break
        except (subprocess.CalledProcessError,subprocess.TimeoutExpired,zipfile.BadZipFile):
            if attempt==2:raise
    prior=json.loads(archived.read("build-manifest.json"));candidate=ROOT.parent/"candidate"
    revision="bf8e8cb19b475cd7b2048272fa30e2a7386f917d"
    assert prior["candidate_revision"]==revision and prior["base_revision"]==BASE
    assert run(["git","-C",candidate,"rev-parse","HEAD"],capture_output=True,text=True).stdout.strip()==revision
    assert digest(candidate/"Dockerfile")==prior["dockerfile_sha256"] and digest(candidate/"Cargo.lock")==prior["lock_sha256"]
    path=candidate/"crates/gatel-bundle-std/src"
    (path/"body_timing.rs").write_bytes(Path(__file__).with_name("body_timing.rs").read_bytes())
    lib=(path/"lib.rs").read_text();assert lib.count("mod file_body;")==1
    (path/"lib.rs").write_text(lib.replace("mod file_body;","mod body_timing;\nmod file_body;"))
    ranges=(path/"ranges.rs").read_text()
    eager="response.body(crate::file_body::stream(file.into_std().await, start, length));"
    assert ranges.count(eager)==1
    ranges=ranges.replace(eager,"response.body(crate::body_timing::wrap(crate::file_body::stream(file.into_std().await, start, length), length));")
    portable="response.body(ResBody::Boxed(Box::pin(http_body_util::StreamBody::new(frames))));"
    assert ranges.count(portable)==1
    ranges=ranges.replace(portable,"let body=ResBody::Boxed(Box::pin(http_body_util::StreamBody::new(frames))); response.body(if length>=8*1024*1024 { crate::body_timing::wrap(body,length) } else { body });")
    (path/"ranges.rs").write_text(ranges)
    patch=run(["git","-C",candidate,"diff","--","crates/gatel-bundle-std/src/lib.rs","crates/gatel-bundle-std/src/ranges.rs"],capture_output=True,text=True).stdout
    (OUT/"diagnostic.patch").write_text(patch);(OUT/"body_timing.rs").write_bytes((path/"body_timing.rs").read_bytes())
    run(["docker","build","--target","build-source","-t","gatel-timing-builder:local",candidate])
    compiler=run(["docker","run","--rm","--entrypoint","rustc","gatel-timing-builder:local","-Vv"],capture_output=True,text=True).stdout
    assert compiler==prior["images"]["candidate"]["compiler"]
    container="copy-body-timing";run(["docker","create","--name",container,"gatel-timing-builder:local"])
    try:run(["docker","cp",container+":/out/gatel",OUT/"timing-gatel"])
    finally:run(["docker","rm",container])
    production=archived.read("candidate-gatel");assert hashlib.sha256(production).hexdigest()==prior["images"]["candidate"]["binary_sha256"]
    (OUT/"production-gatel").write_bytes(production)
    manifest={"base_revision":BASE,"candidate_revision":revision,"execution_revision":os.environ["GITHUB_SHA"],"source_artifact":12906571886,"phase_order":ORDER,"scenarios":SCENARIOS,
        "purpose":"Diagnostic only; classify >=8MiB response body producer-readiness waits versus gaps before the consumer asks for the next frame. Producer wait includes file IO and resume scheduling; consumer gaps include backpressure and connection scheduling. Not final wire completion; cannot identify a unique TCP cause. Paired original production binary calibrates probe effects. Never pool with uninstrumented acceptance data.",
        "patch_sha256":digest(OUT/"diagnostic.patch"),"module_sha256":digest(OUT/"body_timing.rs"),"dockerfile_sha256":prior["dockerfile_sha256"],"lock_sha256":prior["lock_sha256"],
        "images":{"production":prior["images"]["candidate"],"timing":{"compiler":compiler,"binary_sha256":digest(OUT/"timing-gatel")}}}
    (OUT/"build-manifest.json").write_text(json.dumps(manifest,indent=2))
    for name in ["production","timing"]:
        context=OUT/(name+"-context");p=context/"context/linux/amd64/gatel";p.parent.mkdir(parents=True);p.write_bytes((OUT/(name+"-gatel")).read_bytes());(context/"Dockerfile").write_bytes((ROOT/"Dockerfile").read_bytes())
        run(["docker","build","--build-arg","SOURCE=prebuilt","--build-arg","TARGETPLATFORM=linux/amd64","-t","gatel-ab:"+name,context])
    (OUT/"image-inspect.json").write_text(run(["docker","image","inspect","gatel-ab:production","gatel-ab:timing"],capture_output=True,text=True).stdout)
    run(["docker","compose","-f",HERE/"compose.yml","build","bench"])


def launcher():
    path=OUT/"ab-harness.py"
    path.write_text('''import json,os,sys,threading,time,subprocess,copy
from pathlib import Path
sys.path.insert(0,'''+repr(str(HERE))+''')
import run as bench
bench.git_revision=lambda:os.environ["GATEL_AB_REVISION"]
for name,size in [("16k",16384),("32k",32768),("63k",64512),("64k-plus1",65537),("4m",4194304),("20m",20971520)]:
    filename="payload-"+name+".bin"
    bench.PAYLOADS[filename]=size
    bench.SCENARIOS["static-"+name]={"description":"Boundary control "+name,"service":"{target}-static","path":"/"+filename}
original=bench.benchmark_target
resources=[]
resource_path=Path(sys.argv[sys.argv.index("--output-dir")+1])/"process-resources.json"
def measured(url,args,script):
    if hasattr(args,"output_dir") and (url.endswith("/payload-10m.bin") or url.endswith("/payload-20m.bin")) and script is None:
        args=copy.copy(args);args.duration=90
    service="gatel-proxy" if "gatel-proxy" in url else "gatel-static"
    container=bench.compose("ps","-q",service,capture_output=True).stdout.strip()
    pid=subprocess.check_output(["docker","inspect","--format","{{.State.Pid}}",container],text=True).strip()
    bench_id=bench.compose("ps","-q","bench",capture_output=True).stdout.strip()
    bench_pid=subprocess.check_output(["docker","inspect","--format","{{.State.Pid}}",bench_id],text=True).strip()
    processes={service:pid,"bench":bench_pid}
    groups={}
    for name,process in processes.items():
        line=next(x for x in Path("/proc/"+process+"/cgroup").read_text().splitlines() if x.startswith("0::"))
        groups[name]=Path("/sys/fs/cgroup")/line.split("::",1)[1].lstrip("/")
    def counters(process):
        result={}
        for filename in ["snmp","netstat"]:
            lines=subprocess.check_output(["sudo","cat","/proc/"+process+"/net/"+filename],text=True).splitlines()
            for i in range(0,len(lines)-1,2):
                names=lines[i].split();values=lines[i+1].split()
                if names[0]!=values[0]:continue
                for key,value in zip(names[1:],values[1:]):
                    try:result[names[0].rstrip(":")+"/"+key]=int(value)
                    except ValueError:pass
        return result
    before={name:counters(process) for name,process in processes.items()}
    def groupstats(path):
        record={}
        for filename in ["memory.stat","cpu.stat"]:
            try:
                for line in (path/filename).read_text().splitlines():
                    key,value=line.split();record[filename+"/"+key]=int(value)
            except OSError:pass
        return record
    stopped=threading.Event()
    samples=[]
    def sample():
        while not stopped.is_set():
            try:
                status=Path("/proc/"+pid+"/status").read_text()
                rss=int(next(line.split()[1] for line in status.splitlines() if line.startswith("VmRSS:")))*1024
                stat=Path("/proc/"+pid+"/stat").read_text().split()
                samples.append({"monotonic":time.monotonic(),"unix_ms":time.time()*1000,"rss_bytes":rss,"threads":int(next(line.split()[1] for line in status.splitlines() if line.startswith("Threads:"))),"cpu_ticks":int(stat[13])+int(stat[14]),"groups":{name:groupstats(path) for name,path in groups.items()}})
            except (OSError,StopIteration): pass
            stopped.wait(1)
    sampler=threading.Thread(target=sample,daemon=True)
    sampler.start()
    try: return original(url,args,script or "/workspace/tail-stats.lua")
    finally:
        stopped.set();sampler.join()
        after={name:counters(process) for name,process in processes.items()}
        resources.append({"tcp_delta":{name:{key:value-before[name].get(key,0) for key,value in values.items()} for name,values in after.items()},"url":url,"duration":args.duration,"kind":"measurement" if hasattr(args,"output_dir") else "warmup","clock_ticks_per_second":os.sysconf("SC_CLK_TCK"),"samples":samples})
        resource_path.write_text(json.dumps(resources,indent=2))
bench.benchmark_target=measured
raise SystemExit(bench.main())
''')
    return path


def environment(name):
    done = """
done = function(summary, latency, requests)
  io.write(string.format("TAIL_DIAG p999_us=%d p9999_us=%d max_us=%d completed=%d timeout=%d\\n",
    latency:percentile(99.9), latency:percentile(99.99), latency.max, summary.requests, summary.errors.timeout))
end
"""
    mounts=[]
    for filename in ["tail-stats.lua","range-64k.lua","range-offset-64k.lua","range-65k.lua"]:
        prefix = 'wrk.headers["Accept-Encoding"] = "identity"\n' if filename == "tail-stats.lua" else (HERE/"fixtures"/filename).read_text()
        path=OUT/filename
        path.write_text(prefix+done)
        mounts.append("      - "+str(path)+":/workspace/"+filename+":ro")
    override=OUT/(name+"-compose.yml")
    override.write_text("services:\n  gatel-static:\n    image: gatel-ab:"+name+"\n  gatel-proxy:\n    image: gatel-ab:"+name+"\n  bench:\n    volumes:\n"+"\n".join(mounts)+"\n")
    return dict(os.environ,GATEL_AB_REVISION="bf8e8cb+"+name,COMPOSE_FILE=str(HERE/"compose.yml")+":"+str(override))


def measure():
    launch=launcher()
    rows=[]
    for phase,name in enumerate(ORDER,1):
        directory=OUT/f"screen-{phase:02d}-{name}"
        directory.mkdir()
        command=[sys.executable,"-B","-u",launch,"--target","gatel","--no-build","--rounds","1","--duration","20","--warmup","5","--threads","4","--connections","128","--output-dir",directory]
        for scenario in SCENARIOS:command.extend(["--scenario",scenario])
        with (directory/"run.log").open("w") as log:
            process=subprocess.Popen(list(map(str,command)),env=environment(name),stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
            for line in process.stdout:print(line,end="",flush=True);log.write(line);log.flush()
            if process.wait():raise RuntimeError("Screen phase failed")
        part=json.loads((directory/"summary.json").read_text())
        assert len(part)==len(json.loads((directory/"payload-checks.json").read_text()))==2
        for row in part:
            raw=(directory/"raw"/f'{row["scenario"]}-gatel-round1.txt').read_text()
            extra=re.search(r"TAIL_DIAG p999_us=(\d+) p9999_us=(\d+) max_us=(\d+) completed=(\d+) timeout=(\d+)",raw)
            assert extra,raw
            row.update(p999_ms=int(extra[1])/1000,p9999_ms=int(extra[2])/1000,histogram_max_ms=int(extra[3])/1000,variant=name,phase=phase,suite="screen")
            assert int(extra[4])==row["total_requests"] and int(extra[5])==row["timeout_errors"]
        rows.extend(part)
        (OUT/"comparison.json").write_text(json.dumps(rows,indent=2))
        report=directory/"report.md"
        report.write_text(report.read_text().replace("1 round(s) of 20s each", "1 round per case: static-10m/static-20m=90s, other cases=20s"))
    assert len(rows)==8
    summary=[]
    for scenario in SCENARIOS:
        item={"scenario":scenario}
        for name in ["production","timing"]:
            selected=[r for r in rows if r["scenario"]==scenario and r["variant"]==name]
            assert len(selected)==2
            item[name]={k:[r[k] for r in selected] for k in ["requests_per_sec","p99_ms","p999_ms","p9999_ms","histogram_max_ms","timeout_errors","non_2xx_3xx","socket_errors"]}
        summary.append(item)
    (OUT/"analysis.json").write_text(json.dumps(summary,indent=2))
    for item in summary:print(json.dumps(item),flush=True)

OUT.mkdir(parents=True,exist_ok=True)
{"build":build,"measure":measure}[sys.argv[1]]()
