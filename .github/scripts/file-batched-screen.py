"""Screen transport fairness alternatives; production builds and archived baseline."""
import hashlib,io,json,os,re,statistics,subprocess,sys,threading,time,zipfile
from pathlib import Path
ROOT=Path.cwd();HERE=ROOT/"benchmarks/http-servers";OUT=Path(os.environ["BENCHMARK_OUTPUT"])
BASE="f11ffccdaaae6249c1f779160b5b84af1f479ae6"
VARIANTS={"direct16":None,"batch128":131072,"batch256":262144}
ORDER=["base","direct16","batch128","batch256","batch256","batch128","direct16","base"]
SCENARIOS=["static-16k","static-4m","static-10m","static-20m"]

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
    previous=json.loads(archived.read("build-manifest.json"))
    assert previous["base_revision"]==BASE and previous["candidate_revision"]=="bf8e8cb19b475cd7b2048272fa30e2a7386f917d"
    manifest={"base_revision":BASE,"candidate_revision":"4fc25c6079331617aea42169ee6fc4abd2b35b9c","parent_candidate_revision":previous["candidate_revision"],"variants":VARIANTS,"source_artifact":12906571886,
        "execution_revision":os.environ["GITHUB_SHA"],"phase_order":ORDER,"scenarios":SCENARIOS,"case_durations_seconds":{"static-10m":90,"static-20m":90,"other":20},"warmup_seconds":5,
        "purpose":"Diagnostic bounded128/256KiB read batches split into unchanged64KiB frames;16readpermits coverIOonly. Bulkpool8idle buffers; noindependentproducer; noTCP/SDKchange. Compareexactbase andread16 ondefaultclient32MiB/5s; allwindows retained; cleanfinalvalidation required.",
        "build":"Archived freshproductionbase anddirect16; batch128 cleanproductionDockerfile target, batch256 samebuilder oneBULK_READ_SIZEconstantpatch release locked.",
        "dockerfile_sha256":digest(ROOT/"Dockerfile"),"lock_sha256":digest(ROOT/"Cargo.lock"),"images":{}}
    assert manifest["dockerfile_sha256"]==previous["dockerfile_sha256"] and manifest["lock_sha256"]==previous["lock_sha256"]
    source=ROOT.parent/"candidate";stable=ROOT.parent/"stable"
    assert run(["git","-C",source,"rev-parse","HEAD"],capture_output=True,text=True).stdout.strip()==manifest["candidate_revision"]
    assert run(["git","-C",stable,"rev-parse","HEAD"],capture_output=True,text=True).stdout.strip()==previous["candidate_revision"]
    assert digest(source/"Dockerfile")==manifest["dockerfile_sha256"] and digest(source/"Cargo.lock")==manifest["lock_sha256"]
    run(["docker","build","--target","build-source","-t","gatel-batch-builder:batch128",source])
    compiler=run(["docker","run","--rm","--entrypoint","rustc","gatel-batch-builder:batch128","-Vv"],capture_output=True,text=True).stdout
    assert compiler==previous["images"]["base"]["compiler"]
    original=(source/"crates/gatel-bundle-std/src/file_body.rs").read_bytes()
    assert original.count(b"pub(crate) const BULK_READ_SIZE: usize = 128 * 1024;")==1
    assert original.count(b"const BULK_READS: usize = 16;")==1
    for name in ["base",*VARIANTS]:
        if name in ["base","direct16"]:
            parent="base" if name=="base" else "candidate"
            binary=archived.read(parent+"-gatel");assert hashlib.sha256(binary).hexdigest()==previous["images"][parent]["binary_sha256"]
            (OUT/(name+"-gatel")).write_bytes(binary)
            manifest["images"][name]=dict(previous["images"][parent])
            if name=="direct16":
                actual=(stable/"crates/gatel-bundle-std/src/file_body.rs").read_bytes();(OUT/(name+"-file_body.rs")).write_bytes(actual)
                manifest["images"][name].update(file_source_sha256=hashlib.sha256(actual).hexdigest(),read_batch_bytes=None)
        else:
            changed=original if name=="batch128" else original.replace(b"pub(crate) const BULK_READ_SIZE: usize = 128 * 1024;",b"pub(crate) const BULK_READ_SIZE: usize = 256 * 1024;")
            (OUT/(name+"-file_body.rs")).write_bytes(changed)
            if name!="batch128":
                ctx=OUT/(name+"-builder-context");ctx.mkdir();(ctx/"file_body.rs").write_bytes(changed)
                (ctx/"Dockerfile").write_text("FROM gatel-batch-builder:batch128\nCOPY file_body.rs /src/crates/gatel-bundle-std/src/file_body.rs\nRUN touch crates/gatel-bundle-std/src/file_body.rs && cargo build --release --locked -p gatel && cp target/release/gatel /out/gatel\n")
                run(["docker","build","-t","gatel-batch-builder:"+name,ctx])
            builder="gatel-batch-builder:"+name;container="copy-coop-"+name
            run(["docker","create","--name",container,builder])
            try:run(["docker","cp",container+":/out/gatel",OUT/(name+"-gatel")])
            finally:run(["docker","rm",container])
            current=run(["docker","run","--rm","--entrypoint","rustc",builder,"-Vv"],capture_output=True,text=True).stdout;assert current==compiler
            manifest["images"][name]={"compiler":compiler,"binary_sha256":digest(OUT/(name+"-gatel")),"file_source_sha256":hashlib.sha256(changed).hexdigest(),"read_batch_bytes":VARIANTS[name]}
        context=OUT/(name+"-context");p=context/"context/linux/amd64/gatel";p.parent.mkdir(parents=True);p.write_bytes((OUT/(name+"-gatel")).read_bytes());(context/"Dockerfile").write_bytes((ROOT/"Dockerfile").read_bytes())
        run(["docker","build","--build-arg","SOURCE=prebuilt","--build-arg","TARGETPLATFORM=linux/amd64","-t","gatel-ab:"+name,context])
        (OUT/"build-manifest.json").write_text(json.dumps(manifest,indent=2))
    (OUT/"image-inspect.json").write_text(run(["docker","image","inspect",*["gatel-ab:"+n for n in ["base",*VARIANTS]]],capture_output=True,text=True).stdout)
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
    return dict(os.environ,GATEL_AB_REVISION=(BASE if name=="base" else "bf8e8cb19b475cd7b2048272fa30e2a7386f917d" if name=="direct16" else "4fc25c6079331617aea42169ee6fc4abd2b35b9c"),COMPOSE_FILE=str(HERE/"compose.yml")+":"+str(override))


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
        assert len(part)==len(json.loads((directory/"payload-checks.json").read_text()))==4
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
    assert len(rows)==32
    summary=[]
    for scenario in SCENARIOS:
        item={"scenario":scenario}
        for name in ["base",*VARIANTS]:
            selected=[r for r in rows if r["scenario"]==scenario and r["variant"]==name]
            assert len(selected)==2
            item[name]={k:[r[k] for r in selected] for k in ["requests_per_sec","p99_ms","p999_ms","p9999_ms","histogram_max_ms","timeout_errors","non_2xx_3xx","socket_errors"]}
        summary.append(item)
    (OUT/"analysis.json").write_text(json.dumps(summary,indent=2))
    for item in summary:print(json.dumps(item),flush=True)

OUT.mkdir(parents=True,exist_ok=True)
{"build":build,"measure":measure}[sys.argv[1]]()
