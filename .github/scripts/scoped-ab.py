"""Production-build A/B, same runner, balanced phase order, exact byte checks."""
import hashlib,io,json,os,re,statistics,subprocess,sys,threading,time,zipfile
from pathlib import Path
ROOT=Path.cwd()
HERE=ROOT/"benchmarks/http-servers"
OUT=Path(os.environ["BENCHMARK_OUTPUT"])
BASE="f11ffccdaaae6249c1f779160b5b84af1f479ae6"
CANDIDATE="90fff1582053b1811364c0b76cc8bb46d146311a"
SCENARIOS=["static-4k","static-16k","static-32k","static-63k","static-64k","static-64k-plus1","static-65k","range-10m","range-offset-10m","range-65k","static-1m","static-4m","static-10m","proxy","proxy-1m","proxy-10m"]
ORDER=["base","candidate","candidate","base","base","candidate"]
def run(args,**kw):
    print("$"," ".join(map(str,args)),flush=True)
    return subprocess.run(list(map(str,args)),check=True,**kw)
def digest(p): return hashlib.sha256(Path(p).read_bytes()).hexdigest()
def build():
    assert subprocess.check_output(["git","rev-parse","HEAD"],text=True).strip()==BASE
    candidate=ROOT.parent/"candidate"
    assert subprocess.check_output(["git","-C",candidate,"rev-parse","HEAD"],text=True).strip()==CANDIDATE
    for attempt in range(3):
        try:
            payload=subprocess.check_output(["gh","api","repos/salvo-rs/gatel/actions/artifacts/12885226926/zip"],timeout=90)
            archived=zipfile.ZipFile(io.BytesIO(payload));break
        except (subprocess.CalledProcessError,subprocess.TimeoutExpired,zipfile.BadZipFile):
            if attempt==2:raise
    previous=json.loads(archived.read("build-manifest.json"))
    assert previous["base_revision"]==BASE
    binary=archived.read("base-gatel")
    assert hashlib.sha256(binary).hexdigest()==previous["images"]["base"]["binary_sha256"]
    (OUT/"base-gatel").write_bytes(binary)
    context=OUT/"base-context";path=context/"context/linux/amd64/gatel"
    path.parent.mkdir(parents=True);path.write_bytes(binary)
    (context/"Dockerfile").write_bytes((ROOT/"Dockerfile").read_bytes())
    run(["docker","build","--build-arg","SOURCE=prebuilt","--build-arg","TARGETPLATFORM=linux/amd64","-t","gatel-ab:base",context])
    manifest={"base_revision":BASE,"candidate_revision":CANDIDATE,"execution_revision":os.environ["GITHUB_SHA"],"phase_order":ORDER,"scenarios":SCENARIOS,
        "error_policy":"Retain all HTTP/socket/over-limit errors on both revisions through all phases; require zero candidate errors. Default client and unchanged wrk 5s limit. No namespace tuning.",
        "dockerfile_sha256":digest(ROOT/"Dockerfile"),"lock_sha256":digest(ROOT/"Cargo.lock"),
        "build":"Exact archived production baseline12885226926 plus final candidate built with unmodified production Dockerfile SOURCE=source and identical locked compiler/dependencies.",
        "policy":"Only Linux HTTP/1.1 large streamed file responses request bounded output; proxy, small files and H2 retain original socket policy.","source_artifact":12885226926,"images":{"base":previous["images"]["base"]}}
    assert manifest["lock_sha256"]==previous["lock_sha256"] and manifest["dockerfile_sha256"]==previous["dockerfile_sha256"]
    assert digest(candidate/"Dockerfile")==manifest["dockerfile_sha256"] and digest(candidate/"Cargo.lock")==manifest["lock_sha256"]
    run(["docker","build","--target","build-source","-t","gatel-ab-builder:candidate",candidate])
    compiler=run(["docker","run","--rm","--entrypoint","rustc","gatel-ab-builder:candidate","-Vv"],capture_output=True,text=True).stdout
    assert compiler==manifest["images"]["base"]["compiler"]
    run(["docker","build","-t","gatel-ab:candidate",candidate])
    container="gatel-ab-copy-candidate";run(["docker","create","--name",container,"gatel-ab:candidate"])
    try:run(["docker","cp",container+":/usr/local/bin/gatel",OUT/"candidate-gatel"])
    finally:run(["docker","rm",container])
    manifest["images"]["candidate"]={"compiler":compiler,"binary_sha256":digest(OUT/"candidate-gatel")}
    (OUT/"build-manifest.json").write_text(json.dumps(manifest,indent=2))
    (OUT/"image-inspect.json").write_text(run(["docker","image","inspect","gatel-ab:base","gatel-ab:candidate"],capture_output=True,text=True).stdout)
    run(["docker","compose","-f",HERE/"compose.yml","build","bench"])

def launcher():
    path=OUT/"ab-harness.py"
    path.write_text('''import json,os,sys,threading,time,subprocess
from pathlib import Path
sys.path.insert(0,'''+repr(str(HERE))+''')
import run as bench
for name,size in [("16k",16384),("32k",32768),("63k",64512),("64k-plus1",65537),("4m",4194304)]:
    filename="payload-"+name+".bin"
    bench.PAYLOADS[filename]=size
    bench.SCENARIOS["static-"+name]={"description":"Boundary control "+name,"service":"{target}-static","path":"/"+filename}
for suffix in ["1m","10m"]:
    bench.SCENARIOS["proxy-"+suffix]={"description":"Large reverse proxy control "+suffix,"service":"{target}-proxy","path":"/payload-"+suffix+".bin"}
original=bench.benchmark_target
resources=[]
resource_path=Path(sys.argv[sys.argv.index("--output-dir")+1])/"process-resources.json"
def measured(url,args,script):
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
    return dict(os.environ,COMPOSE_FILE=str(HERE/"compose.yml")+":"+str(override))

def measure():
    launch=launcher()
    all_rows=json.loads((OUT/"comparison.json").read_text()) if (OUT/"comparison.json").exists() else []
    for suite,scenarios,duration,warmup in [("matrix",SCENARIOS,20,5),("tail",["static-10m"],120,10),("proxy-tail",["proxy"],60,5)]:
        if len(sys.argv)>2 and sys.argv[2]!=suite:continue
        for phase,name in enumerate(ORDER,1):
            if len(sys.argv)>3 and phase!=int(sys.argv[3]):continue
            directory=OUT/f"{suite}-{phase:02d}-{name}"
            directory.mkdir()
            command=[sys.executable,"-B","-u",launch,"--target","gatel","--no-build","--rounds","1","--duration",duration,"--warmup",warmup,"--threads","4","--connections","128","--output-dir",directory]
            for scenario in scenarios: command.extend(["--scenario",scenario])
            print(f"SUITE {suite} PHASE {phase}/6 {name}",flush=True)
            with (directory/"run.log").open("w") as log:
                process=subprocess.Popen(list(map(str,command)),env=environment(name),stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
                for line in process.stdout:
                    print(line,end="",flush=True);log.write(line);log.flush()
                if process.wait(): raise RuntimeError("A/B phase failed")
            rows=json.loads((directory/"summary.json").read_text())
            checks=json.loads((directory/"payload-checks.json").read_text())
            assert len(rows)==len(scenarios)==len(checks)
            for row in rows:
                raw=(directory/"raw"/f'{row["scenario"]}-gatel-round1.txt').read_text()
                sockets=re.search(r"Socket errors:.*timeout\s+(\d+)",raw)
                row["timeout_errors"]=int(sockets.group(1)) if sockets else 0
                extra=re.search(r"TAIL_DIAG p999_us=(\d+) p9999_us=(\d+) max_us=(\d+) completed=(\d+) timeout=(\d+)",raw)
                assert extra, raw
                row.update(p999_ms=int(extra[1])/1000,p9999_ms=int(extra[2])/1000,histogram_max_ms=int(extra[3])/1000)
                assert int(extra[4])==row["total_requests"] and int(extra[5])==row["timeout_errors"],row
                row.update(variant=name,phase=phase,suite=suite)
            all_rows.extend(rows)
            (OUT/"comparison.json").write_text(json.dumps(all_rows,indent=2))
    (OUT/"runtime-image-inspect.json").write_text(run(["docker","image","inspect","gatel-http-bench-runner:local","nginx:1.27"],capture_output=True,text=True).stdout)
    if len(all_rows)<108:
        print("Saved partial suite",sys.argv[2],len(all_rows),flush=True);return
    assert len(all_rows)==108
    summary=[]
    for suite in ["matrix","tail","proxy-tail"]:
        for scenario in SCENARIOS if suite=="matrix" else ["static-10m"] if suite=="tail" else ["proxy"]:
            row={"suite":suite,"scenario":scenario}
            for name in ["base","candidate"]:
                selected=[r for r in all_rows if r["variant"]==name and r["suite"]==suite and r["scenario"]==scenario]
                assert len(selected)==3
                row[name]={metric:[r[metric] for r in selected] for metric in ["requests_per_sec","p99_ms","latency_avg_ms","latency_max_ms","total_requests","socket_errors","non_2xx_3xx","timeout_errors","p999_ms","p9999_ms","histogram_max_ms"]}
                row[name]["rps_mean"]=statistics.mean(row[name]["requests_per_sec"])
                row[name]["p99_mean"]=statistics.mean(row[name]["p99_ms"])
            row["rps_change_pct"]=(row["candidate"]["rps_mean"]/row["base"]["rps_mean"]-1)*100
            row["p99_change_pct"]=(row["candidate"]["p99_mean"]/row["base"]["p99_mean"]-1)*100
            summary.append(row)
    (OUT/"analysis.json").write_text(json.dumps(summary,indent=2))
    for row in summary: print(json.dumps(row),flush=True)
    assert all(row["socket_errors"]==row["non_2xx_3xx"]==0 for row in all_rows if row["variant"]=="candidate"),"Candidate has HTTP/socket errors; full results retained, do not accept."
OUT.mkdir(parents=True,exist_ok=True)
{"build":build,"measure":measure}[sys.argv[1]]()

