"""Same-binary transport diagnostic; tracing is separate from acceptance."""
import hashlib,io,json,os,shutil,subprocess,sys,zipfile
from pathlib import Path
ROOT=Path.cwd();HERE=ROOT/"benchmarks/http-servers";OUT=Path(os.environ["BENCHMARK_OUTPUT"]);OUT.mkdir(parents=True,exist_ok=True)
def run(args,**kw):
    print("$"," ".join(map(str,args)),flush=True)
    return subprocess.run(list(map(str,args)),check=True,**kw)
def build():
    result=run(["gh","api","repos/salvo-rs/gatel/actions/artifacts/12865677420/zip"],capture_output=True)
    source=OUT/"source-artifact";source.mkdir()
    with zipfile.ZipFile(io.BytesIO(result.stdout)) as archive:
        for member in archive.infolist(): assert (source/member.filename).resolve().is_relative_to(source.resolve())
        archive.extractall(source)
    manifest=json.loads((source/"build-manifest.json").read_text())
    for name,key in [("base","base"),("batch","candidate")]:
        directory=OUT/("build-"+name);target=directory/"context/linux/amd64/gatel";target.parent.mkdir(parents=True)
        binary=source/(key+"-gatel")
        assert hashlib.sha256(binary.read_bytes()).hexdigest()==manifest["images"][key]["binary_sha256"]
        shutil.copyfile(binary,target);shutil.copyfile(ROOT/"Dockerfile",directory/"Dockerfile")
        run(["docker","build","--build-arg","SOURCE=prebuilt","--build-arg","TARGETPLATFORM=linux/amd64","-t","gatel-transport:"+name,directory])
    (OUT/"provenance.json").write_text(json.dumps({"source_artifact":12865677420,"source_run":40797322335,"execution":os.environ["GITHUB_SHA"],"manifest":manifest},indent=2))
    run(["docker","compose","-f",HERE/"compose.yml","build","bench"])
def launcher():
    path=OUT/"transport-harness.py"
    path.write_text('''import json,os,sys,subprocess,threading,time
from pathlib import Path
sys.path.insert(0,'''+repr(str(HERE))+''')
import run as bench
original=bench.benchmark_target
destination=Path(sys.argv[sys.argv.index("--output-dir")+1])
records=[]
def command(args):
    p=subprocess.run(args,capture_output=True,text=True)
    return p.stdout if p.returncode==0 else "ERROR: "+p.stderr
def counters(pid):
    result={}
    for file in ["snmp","netstat"]:
        raw=command(["sudo","cat","/proc/"+pid+"/net/"+file])
        lines=raw.splitlines()
        for i in range(0,len(lines)-1,2):
            names=lines[i].split();values=lines[i+1].split()
            if names[0]!=values[0]: continue
            for name,value in zip(names[1:],values[1:]):
                try: result[names[0].rstrip(":")+"/"+name]=int(value)
                except ValueError: pass
    return result
def group(pid):
    line=next(x for x in Path("/proc/"+pid+"/cgroup").read_text().splitlines() if x.startswith("0::"))
    return Path("/sys/fs/cgroup")/line.split("::",1)[1].lstrip("/")
def cgstats(path):
    out={}
    for file in ["cpu.stat","memory.stat"]:
        try:
            for line in (path/file).read_text().splitlines():
                key,value=line.split()
                out[file+"/"+key]=int(value)
        except OSError: pass
    return out
def measured(url,args,script):
    if not hasattr(args,"output_dir"): return original(url,args,script)
    service="nginx-static" if "nginx-static" in url else "gatel-static"
    ids={name:bench.compose("ps","-q",name,capture_output=True).stdout.strip() for name in [service,"bench"]}
    pids={name:subprocess.check_output(["docker","inspect","--format","{{.State.Pid}}",identity],text=True).strip() for name,identity in ids.items()}
    cg={name:group(pid) for name,pid in pids.items()}
    before={name:counters(pid) for name,pid in pids.items()}
    for name,pid in pids.items():
        (destination/(name+"-sysctl.txt")).write_text(command(["sudo","nsenter","-t",pid,"-n","sysctl","net.ipv4.tcp_rmem","net.ipv4.tcp_wmem","net.ipv4.tcp_mem","net.ipv4.tcp_moderate_rcvbuf","net.ipv4.tcp_notsent_lowat"]))
    (destination/"host-meminfo.txt").write_text(Path("/proc/meminfo").read_text())
    cgbefore={name:cgstats(path) for name,path in cg.items()}
    softbefore=Path("/proc/net/softnet_stat").read_text()
    stop=threading.Event();samples=[]
    def sample():
        midpoint=False;start=time.monotonic()
        while not stop.is_set():
            point={"at":time.monotonic(),"groups":{name:cgstats(path) for name,path in cg.items()}}
            for name,pid in pids.items():
                try:
                    status=Path("/proc/"+pid+"/status").read_text()
                    point[name]={key:next(x.split()[1] for x in status.splitlines() if x.startswith(key+":")) for key in ["VmRSS","Threads"]}
                except (OSError,StopIteration): pass
            samples.append(point)
            if not midpoint and time.monotonic()-start>args.duration/2:
                midpoint=True
                for name,pid in pids.items():
                    (destination/(name+"-ss.txt")).write_text(command(["sudo","nsenter","-t",pid,"-n","ss","-tinm","state","established"]))
                (destination/"host-netdev.txt").write_text(Path("/proc/net/dev").read_text())
            stop.wait(1)
    thread=threading.Thread(target=sample,daemon=True);thread.start()
    try: return original(url,args,script)
    finally:
        stop.set();thread.join()
        after={name:counters(pid) for name,pid in pids.items()}
        cgafter={name:cgstats(path) for name,path in cg.items()}
        records.append({"url":url,"duration":args.duration,"before":before,"after":after,"tcp_delta":{name:{key:values[key]-before[name].get(key,0) for key in values} for name,values in after.items()},"cgroup_before":cgbefore,"cgroup_after":cgafter,"samples":samples,"softnet_before":softbefore,"softnet_after":Path("/proc/net/softnet_stat").read_text()})
        (destination/"transport.json").write_text(json.dumps(records,indent=2))
bench.benchmark_target=measured
raise SystemExit(bench.main())
''')
    return path
def measure():
    adapter=launcher();all_rows=[]
    order=["base-default","base-bounded","batch-bounded","batch-default","batch-default","batch-bounded","base-bounded","base-default"]
    for phase,label in enumerate(order,1):
        name,mode=label.split("-")
        target="nginx" if name=="nginx" else "gatel"
        directory=OUT/f"{phase:02d}-{label}";directory.mkdir()
        env=dict(os.environ)
        if target=="gatel":
            override=OUT/(name+".yml")
            override.write_text("services:\n  gatel-static:\n    image: gatel-transport:"+name+"\n  gatel-proxy:\n    image: gatel-transport:"+name+"\n")
            if mode=="bounded":
                with override.open("a") as handle:
                    handle.write("  bench:\n    sysctls:\n      net.ipv4.tcp_rmem: '4096 131072 524288'\n")
            env["COMPOSE_FILE"]=str(HERE/"compose.yml")+":"+str(override)
        command=[sys.executable,"-B","-u",adapter,"--target",target,"--no-build","--rounds","1","--scenario","static-10m","--duration","60","--warmup","10","--threads","4","--connections","128","--output-dir",directory]
        print(f"MEMORY PHASE {phase}/8 {label}",flush=True)
        with (directory/"run.log").open("w") as log:
            process=subprocess.Popen(list(map(str,command)),env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
            for line in process.stdout: print(line,end="",flush=True);log.write(line);log.flush()
            assert process.wait()==0
        row=json.loads((directory/"summary.json").read_text())[0];row.update(variant=name,receive_mode=mode,phase=phase);all_rows.append(row)
        (OUT/"comparison.json").write_text(json.dumps(all_rows,indent=2))
    print(json.dumps(all_rows,indent=2),flush=True)
{"build":build,"measure":measure}[sys.argv[1]]()

