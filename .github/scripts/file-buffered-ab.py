"""Production-build A/B, same runner, balanced phase order, exact byte checks."""
import hashlib,json,os,statistics,subprocess,sys,threading,time
from pathlib import Path
ROOT=Path.cwd()
HERE=ROOT/"benchmarks/http-servers"
OUT=Path(os.environ["BENCHMARK_OUTPUT"])
BASE="1bbdebfb67a9611e56f798ae9f3afd7275743eb1"
CANDIDATE="32923641efe8beb808020a0d1852370dfe594efd"
SCENARIOS=["static-4k","static-16k","static-32k","static-63k","static-64k","static-64k-plus1","static-65k","range-10m","range-offset-10m","range-65k","static-1m","static-10m","proxy"]
ORDER=["base","candidate","candidate","base","base","candidate"]
def run(args,**kw):
    print("$"," ".join(map(str,args)),flush=True)
    return subprocess.run(list(map(str,args)),check=True,**kw)
def digest(p): return hashlib.sha256(Path(p).read_bytes()).hexdigest()
def build():
    assert subprocess.check_output(["git","rev-parse","HEAD"],text=True).strip()==BASE
    candidate=ROOT.parent/"candidate"
    assert subprocess.check_output(["git","-C",candidate,"rev-parse","HEAD"],text=True).strip()==CANDIDATE
    manifest={"base_revision":BASE,"candidate_revision":CANDIDATE,"execution_revision":os.environ["GITHUB_SHA"],"phase_order":ORDER,"dockerfile_sha256":digest(ROOT/"Dockerfile"),"lock_sha256":digest(ROOT/"Cargo.lock"),"build":"Unmodified production Dockerfile, SOURCE=source, default locked release build for both revisions.","images":{}}
    assert digest(candidate/"Dockerfile")==manifest["dockerfile_sha256"]
    assert digest(candidate/"Cargo.lock")==manifest["lock_sha256"]
    for name,source in [("base",ROOT),("candidate",candidate)]:
        run(["docker","build","--target","build-source","-t","gatel-ab-builder:"+name,source])
        compiler=run(["docker","run","--rm","--entrypoint","rustc","gatel-ab-builder:"+name,"-Vv"],capture_output=True,text=True).stdout
        run(["docker","build","-t","gatel-ab:"+name,source])
        container="gatel-ab-copy-"+name
        run(["docker","create","--name",container,"gatel-ab:"+name])
        binary=OUT/(name+"-gatel")
        try: run(["docker","cp",container+":/usr/local/bin/gatel",binary])
        finally: run(["docker","rm",container])
        manifest["images"][name]={"compiler":compiler,"binary_sha256":digest(binary)}
        binary.unlink()
        (OUT/"build-manifest.json").write_text(json.dumps(manifest,indent=2))
    assert manifest["images"]["base"]["compiler"]==manifest["images"]["candidate"]["compiler"]
    (OUT/"image-inspect.json").write_text(run(["docker","image","inspect","gatel-ab:base","gatel-ab:candidate"],capture_output=True,text=True).stdout)
    run(["docker","compose","-f",HERE/"compose.yml","build","bench"])
def launcher():
    path=OUT/"ab-harness.py"
    path.write_text('''import json,os,sys,threading,time,subprocess
from pathlib import Path
sys.path.insert(0,'''+repr(str(HERE))+''')
import run as bench
for name,size in [("16k",16384),("32k",32768),("63k",64512),("64k-plus1",65537)]:
    filename="payload-"+name+".bin"
    bench.PAYLOADS[filename]=size
    bench.SCENARIOS["static-"+name]={"description":"Boundary control "+name,"service":"{target}-static","path":"/"+filename}
original=bench.benchmark_target
resources=[]
resource_path=Path(sys.argv[sys.argv.index("--output-dir")+1])/"process-resources.json"
def measured(url,args,script):
    service="gatel-proxy" if "gatel-proxy" in url else "gatel-static"
    container=bench.compose("ps","-q",service,capture_output=True).stdout.strip()
    pid=subprocess.check_output(["docker","inspect","--format","{{.State.Pid}}",container],text=True).strip()
    stopped=threading.Event()
    samples=[]
    def sample():
        while not stopped.is_set():
            try:
                status=Path("/proc/"+pid+"/status").read_text()
                rss=int(next(line.split()[1] for line in status.splitlines() if line.startswith("VmRSS:")))*1024
                stat=Path("/proc/"+pid+"/stat").read_text().split()
                samples.append({"monotonic":time.monotonic(),"rss_bytes":rss,"cpu_ticks":int(stat[13])+int(stat[14])})
            except (OSError,StopIteration): pass
            stopped.wait(1)
    sampler=threading.Thread(target=sample,daemon=True)
    sampler.start()
    try: return original(url,args,script)
    finally:
        stopped.set();sampler.join()
        resources.append({"url":url,"duration":args.duration,"kind":"measurement" if hasattr(args,"output_dir") else "warmup","clock_ticks_per_second":os.sysconf("SC_CLK_TCK"),"samples":samples})
        resource_path.write_text(json.dumps(resources,indent=2))
bench.benchmark_target=measured
raise SystemExit(bench.main())
''')
    return path
def environment(name):
    override=OUT/(name+"-compose.yml")
    override.write_text("services:\n  gatel-static:\n    image: gatel-ab:"+name+"\n  gatel-proxy:\n    image: gatel-ab:"+name+"\n")
    return dict(os.environ,COMPOSE_FILE=str(HERE/"compose.yml")+":"+str(override))
def measure():
    launch=launcher()
    all_rows=[]
    for suite,scenarios,duration,warmup in [("matrix",SCENARIOS,20,5),("tail",["static-10m"],60,10)]:
        for phase,name in enumerate(ORDER,1):
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
                assert row["non_2xx_3xx"]==0 and row["socket_errors"]==0,row
                row.update(variant=name,phase=phase,suite=suite)
            all_rows.extend(rows)
            (OUT/"comparison.json").write_text(json.dumps(all_rows,indent=2))
    (OUT/"runtime-image-inspect.json").write_text(run(["docker","image","inspect","gatel-http-bench-runner:local","nginx:1.27"],capture_output=True,text=True).stdout)
    assert len(all_rows)==84
    summary=[]
    for suite in ["matrix","tail"]:
        for scenario in SCENARIOS if suite=="matrix" else ["static-10m"]:
            row={"suite":suite,"scenario":scenario}
            for name in ["base","candidate"]:
                selected=[r for r in all_rows if r["variant"]==name and r["suite"]==suite and r["scenario"]==scenario]
                assert len(selected)==3
                row[name]={metric:[r[metric] for r in selected] for metric in ["requests_per_sec","p99_ms","latency_avg_ms","total_requests"]}
                row[name]["rps_mean"]=statistics.mean(row[name]["requests_per_sec"])
                row[name]["p99_mean"]=statistics.mean(row[name]["p99_ms"])
            row["rps_change_pct"]=(row["candidate"]["rps_mean"]/row["base"]["rps_mean"]-1)*100
            row["p99_change_pct"]=(row["candidate"]["p99_mean"]/row["base"]["p99_mean"]-1)*100
            summary.append(row)
    (OUT/"analysis.json").write_text(json.dumps(summary,indent=2))
    for row in summary: print(json.dumps(row),flush=True)
OUT.mkdir(parents=True,exist_ok=True)
{"build":build,"measure":measure}[sys.argv[1]]()

