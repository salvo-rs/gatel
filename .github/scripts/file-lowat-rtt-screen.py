"""Screen transport fairness alternatives; production builds and archived baseline."""
import hashlib,io,json,os,re,statistics,subprocess,sys,threading,time,zipfile
from pathlib import Path
ROOT=Path.cwd();HERE=ROOT/"benchmarks/http-servers";OUT=Path(os.environ["BENCHMARK_OUTPUT"])
BASE="f11ffccdaaae6249c1f779160b5b84af1f479ae6"
VARIANTS={"low64":65536,"low256":262144}
ORDER=["base","low64","low256","low256","low64","base"]
SCENARIOS=["static-64k","static-1m","static-10m"]
def run(args,**kw):
    print("$"," ".join(map(str,args)),flush=True)
    return subprocess.run(list(map(str,args)),check=True,**kw)


def digest(p): return hashlib.sha256(Path(p).read_bytes()).hexdigest()


def build():
    archived=zipfile.ZipFile(io.BytesIO(run(["gh","api","repos/salvo-rs/gatel/actions/artifacts/12885226926/zip"],capture_output=True).stdout))
    previous=json.loads(archived.read("build-manifest.json"))
    assert previous["base_revision"]==BASE
    binary=archived.read("base-gatel")
    assert hashlib.sha256(binary).hexdigest()==previous["images"]["base"]["binary_sha256"]
    (OUT/"base-gatel").write_bytes(binary)
    context=OUT/"base-context";path=context/"context/linux/amd64/gatel"
    path.parent.mkdir(parents=True);path.write_bytes(binary)
    (context/"Dockerfile").write_bytes((ROOT/"Dockerfile").read_bytes())
    run(["docker","build","--build-arg","SOURCE=prebuilt","--build-arg","TARGETPLATFORM=linux/amd64","-t","gatel-ab:base",context])
    for name in VARIANTS:run(["docker","tag","gatel-ab:base","gatel-ab:"+name])
    manifest={"base_revision":BASE,"variants":VARIANTS,"source_artifact":12885226926,
        "execution_revision":os.environ["GITHUB_SHA"],"phase_order":ORDER,"scenarios":SCENARIOS,
        "case_durations_seconds":{"static-10m":30,"other":10},"warmup_seconds":3,"connections":1,"threads":1,"injected_rtt_ms":int(os.environ["RTT_MS"]),
        "purpose":"Diagnostic only: identical archived production binary, vary only server namespace tcp_notsent_lowat. No client settings or timeouts changed. Full production A/B still required.",
        "dockerfile_sha256":digest(ROOT/"Dockerfile"),"lock_sha256":digest(ROOT/"Cargo.lock"),
        "images":{name:previous["images"]["base"] for name in ["base",*VARIANTS]}}
    assert manifest["lock_sha256"]==previous["lock_sha256"]
    assert manifest["dockerfile_sha256"]==previous["dockerfile_sha256"]
    (OUT/"build-manifest.json").write_text(json.dumps(manifest,indent=2))
    inspected=run(["docker","image","inspect",*["gatel-ab:"+n for n in ["base",*VARIANTS]]],capture_output=True,text=True).stdout
    assert len({i["Id"] for i in json.loads(inspected)})==1
    (OUT/"image-inspect.json").write_text(inspected)
    run(["docker","compose","-f",HERE/"compose.yml","build","bench"])



def launcher():
    path=OUT/"ab-harness.py"
    path.write_text('''import json,os,sys,threading,time,subprocess,copy
from pathlib import Path
sys.path.insert(0,'''+repr(str(HERE))+''')
import run as bench
for name,size in [("16k",16384),("32k",32768),("63k",64512),("64k-plus1",65537),("4m",4194304)]:
    filename="payload-"+name+".bin"
    bench.PAYLOADS[filename]=size
    bench.SCENARIOS["static-"+name]={"description":"Boundary control "+name,"service":"{target}-static","path":"/"+filename}
original=bench.benchmark_target
resources=[]
resource_path=Path(sys.argv[sys.argv.index("--output-dir")+1])/"process-resources.json"
def measured(url,args,script):
    if hasattr(args,"output_dir") and url.endswith("/payload-10m.bin") and script is None:
        args=copy.copy(args);args.duration=30
    service="gatel-proxy" if "gatel-proxy" in url else "gatel-static"
    container=bench.compose("ps","-q",service,capture_output=True).stdout.strip()
    pid=subprocess.check_output(["docker","inspect","--format","{{.State.Pid}}",container],text=True).strip()
    actual=int(subprocess.check_output(["sudo","nsenter","-t",pid,"-n","sysctl","-n","net.ipv4.tcp_notsent_lowat"],text=True))
    expected=os.environ["EXPECTED_SERVER_LOWAT"]
    if expected!="default":assert actual==int(expected),(actual,expected)
    network_file=resource_path.parent/"server-network-settings.json"
    network_file.write_text(json.dumps({"tcp_notsent_lowat":actual,"profile":expected}))
    bench_id=bench.compose("ps","-q","bench",capture_output=True).stdout.strip()
    bench_pid=subprocess.check_output(["docker","inspect","--format","{{.State.Pid}}",bench_id],text=True).strip()
    profile_path=resource_path.parent/"netem-profile.json"
    if not profile_path.exists():
        rtt=int(os.environ["RTT_MS"])
        commands=[]
        if rtt:
            for process in [pid,bench_pid]:
                command=["sudo","nsenter","-t",process,"-n","tc","qdisc","replace","dev","eth0","root","netem","limit","10000","delay",str(rtt/2)+"ms"]
                subprocess.run(command,check=True,capture_output=True,text=True)
                commands.append(command)
        profile_path.write_text(json.dumps({"injected_rtt_ms":rtt,"commands":commands}))
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
    if name in VARIANTS:
        original=override.read_text()
        prefix="  gatel-static:\n    image: gatel-ab:"+name+"\n"
        override.write_text(original.replace(prefix,prefix+"    sysctls:\n      net.ipv4.tcp_notsent_lowat: '"+str(VARIANTS[name])+"'\n"))
    return dict(os.environ,EXPECTED_SERVER_LOWAT=str(VARIANTS.get(name,"default")),COMPOSE_FILE=str(HERE/"compose.yml")+":"+str(override))


def measure():
    launch=launcher()
    rows=[]
    for phase,name in enumerate(ORDER,1):
        directory=OUT/f"screen-{phase:02d}-{name}"
        directory.mkdir()
        command=[sys.executable,"-B","-u",launch,"--target","gatel","--no-build","--rounds","1","--duration","10","--warmup","3","--threads","1","--connections","1","--output-dir",directory]
        for scenario in SCENARIOS:command.extend(["--scenario",scenario])
        with (directory/"run.log").open("w") as log:
            process=subprocess.Popen(list(map(str,command)),env=environment(name),stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
            for line in process.stdout:print(line,end="",flush=True);log.write(line);log.flush()
            if process.wait():raise RuntimeError("Screen phase failed")
        part=json.loads((directory/"summary.json").read_text())
        assert len(part)==len(json.loads((directory/"payload-checks.json").read_text()))==3
        for row in part:
            raw=(directory/"raw"/f'{row["scenario"]}-gatel-round1.txt').read_text()
            extra=re.search(r"TAIL_DIAG p999_us=(\d+) p9999_us=(\d+) max_us=(\d+) completed=(\d+) timeout=(\d+)",raw)
            assert extra,raw
            row.update(p999_ms=int(extra[1])/1000,p9999_ms=int(extra[2])/1000,histogram_max_ms=int(extra[3])/1000,variant=name,phase=phase,suite="screen")
            assert int(extra[4])==row["total_requests"] and int(extra[5])==row["timeout_errors"]
        rows.extend(part)
        (OUT/"comparison.json").write_text(json.dumps(rows,indent=2))
        report=directory/"report.md"
        report.write_text(report.read_text().replace("1 round(s) of 10s each", "1 round per case: static-10m=30s, other cases=10s"))
    assert len(rows)==18
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
