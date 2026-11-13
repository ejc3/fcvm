#!/usr/bin/env python3
"""Prepare the AWS side of a boot-race campaign (run on the campaign box, with boot-race-driver credentials).

    prepare.py --config cfg.json image        push localhost/boot-race-workload to the boot-race ECR repository
    prepare.py --config cfg.json lambda       upload the workload as both functions' bootstrap
    prepare.py --config cfg.json ami          make the EC2 lanes' AMI: base AL2023 + the workload as a systemd unit
    prepare.py --config cfg.json instances    launch the start and hibernate lanes' instances and stop them
    prepare.py --config cfg.json ecs-host     launch the ECS-on-EC2 container instance and pull the image on it
    prepare.py --config cfg.json cleanup      terminate every Project=boot-race instance, deregister the AMI
    prepare.py --config cfg.json leftovers    fail unless nothing the campaign made is left billing

Each step records what it made in the config file, which race.py reads.
"""

import argparse
import base64
import io
import json
import os
import subprocess
import sys
import time
import zipfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import race  # noqa: E402

KEYDIR = "/dev/shm/boot-race-ssh"


def save(path, cfg):
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(cfg, f, indent=1)
    os.replace(tmp, path)


def wait_answer(ip, timeout=600):
    import asyncio

    return asyncio.run(race.probe(ip, 8080, 0.5, timeout))[1]


def image(cfg, s):
    ecr = s.client("ecr")
    tok = ecr.get_authorization_token()["authorizationData"][0]
    user, pw = base64.b64decode(tok["authorizationToken"]).decode().split(":", 1)
    registry = tok["proxyEndpoint"].replace("https://", "")
    repo = f"{registry}/boot-race"
    subprocess.run(["podman", "login", "-u", user, "--password-stdin", registry], input=pw.encode(), check=True)
    subprocess.run(["podman", "tag", "localhost/boot-race-workload", f"{repo}:workload"], check=True)
    subprocess.run(["podman", "push", f"{repo}:workload"], check=True)
    d = ecr.describe_images(repositoryName="boot-race", imageIds=[{"imageTag": "workload"}])["imageDetails"][0]
    race.log(f"pushed {d['imageDigest']} ({d['imageSizeInBytes']} bytes)")


def lambda_code(cfg, s):
    lam = s.client("lambda")
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        info = zipfile.ZipInfo("bootstrap")
        info.external_attr = 0o100755 << 16
        with open(cfg["workload_bin"], "rb") as f:
            z.writestr(info, f.read())
    urls = {}
    for k in ("cold", "warm"):
        name = f"boot-race-{k}"
        lam.update_function_code(FunctionName=name, ZipFile=buf.getvalue())
        lam.get_waiter("function_updated_v2").wait(FunctionName=name)
        urls[k] = lam.get_function_url_config(FunctionName=name)["FunctionUrl"]
        env = lam.get_function_configuration(FunctionName=name)["Environment"]["Variables"]
        race.log(f"{name}: code updated, env {env}")
    cfg["function_urls"] = urls
    # Prime the warm function: the warm lane's first sample must come from this same runtime process.
    lam_lane = race.Lambda(cfg)
    first = lam_lane.call(urls["warm"])["answer"]
    again = lam_lane.call(urls["warm"])["answer"]
    if first["proc_id"] != again["proc_id"]:
        raise SystemExit("the warm function answered from two processes in a row")
    cfg.setdefault("identities", {})["lambda-warm"] = again


def ssh_key():
    os.makedirs(KEYDIR, mode=0o700, exist_ok=True)
    key = f"{KEYDIR}/id"
    if not os.path.exists(key):
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", key], check=True)
    with open(key + ".pub") as f:
        return key, f.read().strip()


def ssh(key, ip, cmd, stdin=None):
    return subprocess.run(
        ["ssh", "-i", key, "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
         "-o", "ConnectTimeout=5", "-o", "BatchMode=yes", f"ec2-user@{ip}", cmd],
        input=stdin, check=True, capture_output=True,
    )


def ami(cfg, s):
    ec2 = s.client("ec2")
    key, pub = ssh_key()
    ud = f"#cloud-config\nssh_authorized_keys:\n  - {pub}\n"
    r = ec2.run_instances(
        LaunchTemplate={"LaunchTemplateName": "boot-race-ec2"},
        MinCount=1, MaxCount=1,
        HibernationOptions={"Configured": False},
        UserData=ud,
        TagSpecifications=race.tags("boot-race-ami-builder"),
    )
    iid, ip = r["Instances"][0]["InstanceId"], r["Instances"][0]["PrivateIpAddress"]
    race.log("builder launched")
    try:
        for _ in range(120):
            try:
                ssh(key, ip, "true")
                break
            except subprocess.CalledProcessError:
                time.sleep(5)
        with open(cfg["workload_bin"], "rb") as f:
            ssh(key, ip, "sudo install -m 0755 /dev/stdin /usr/local/bin/boot-race-workload", stdin=f.read())
        with open(cfg["workload_unit"], "rb") as f:
            ssh(key, ip, "sudo install -m 0644 /dev/stdin /etc/systemd/system/boot-race.service", stdin=f.read())
        ssh(key, ip, "sudo systemctl daemon-reload && sudo systemctl enable --now boot-race.service")
        wait_answer(ip, 60)
        # The image must not carry this session's key; cloud-init installs the instance's own at each launch.
        ssh(key, ip, "sudo rm -f /home/ec2-user/.ssh/authorized_keys")
        name = f"boot-race-workload-{time.strftime('%Y%m%d-%H%M%S')}"
        img = ec2.create_image(
            InstanceId=iid, Name=name, Description="boot-race: AL2023 arm64 + the workload as a systemd unit",
            TagSpecifications=[{"ResourceType": rt, "Tags": [race.TAG, {"Key": "Name", "Value": name}]}
                               for rt in ("image", "snapshot")],
        )["ImageId"]
        ec2.get_waiter("image_available").wait(ImageIds=[img], WaiterConfig={"Delay": 10, "MaxAttempts": 120})
        cfg["ami"] = img
        cfg["ami_name"] = name
        race.log(f"AMI {name} available")
    finally:
        ec2.terminate_instances(InstanceIds=[iid])


def instances(cfg, s):
    ec2 = s.client("ec2")
    made = {}
    for lane, hib in (("start_instance", False), ("hibernate_instance", True)):
        r = ec2.run_instances(
            LaunchTemplate={"LaunchTemplateName": "boot-race-ec2"}, ImageId=cfg["ami"], MinCount=1, MaxCount=1,
            HibernationOptions={"Configured": hib},
            TagSpecifications=race.tags(f"boot-race-{lane.replace('_', '-')}"),
        )
        made[lane] = (r["Instances"][0]["InstanceId"], r["Instances"][0]["PrivateIpAddress"], hib)
    answers = {}
    for lane, (iid, ip, hib) in made.items():
        answers[lane] = wait_answer(ip)
        race.log(f"{lane} answers")
    # AWS asks for a few minutes after launch before the first hibernation, for the guest agent to prepare.
    time.sleep(240)
    ids = cfg.setdefault("identities", {})
    for lane, (iid, ip, hib) in made.items():
        # The identity just before the stop: a hibernate resume must answer from this same process.
        ids["ec2-hibernate" if hib else "ec2-start"] = wait_answer(ip, 10)
        ec2.stop_instances(InstanceIds=[iid], Hibernate=hib)
        cfg[lane] = iid
    for lane, (iid, ip, hib) in made.items():
        ec2.get_waiter("instance_stopped").wait(InstanceIds=[iid])
        race.log(f"{lane} stopped (hibernate={hib})")


def ecs_host(cfg, s):
    ec2, ecs = s.client("ec2"), s.client("ecs")
    r = ec2.run_instances(
        LaunchTemplate={"LaunchTemplateName": "boot-race-ecs-host"}, MinCount=1, MaxCount=1,
        TagSpecifications=race.tags("boot-race-ecs-host"),
    )
    cfg["ecs_host_instance"] = r["Instances"][0]["InstanceId"]
    cfg["ecs_host_ip"] = r["Instances"][0]["PrivateIpAddress"]
    for _ in range(120):
        if ecs.list_container_instances(cluster="boot-race")["containerInstanceArns"]:
            break
        time.sleep(5)
    else:
        raise SystemExit("container instance never registered")
    race.log("container instance registered; pulling the image with one task through the launcher")
    lane = race.Ecs(cfg)
    launch, _ = lane.run({"lane": "ec2", "instance": cfg["ecs_host_instance"]})
    arn = launch.out["task_arn"]
    try:
        cfg.setdefault("identities", {})["ecs-ec2"] = wait_answer(cfg["ecs_host_ip"])
    finally:
        lane.stop(arn)
        ecs.get_waiter("tasks_stopped").wait(cluster="boot-race", tasks=[arn])
    race.log("ECS host warm")


def cleanup(cfg, s):
    ec2 = s.client("ec2")
    r = ec2.describe_instances(Filters=[{"Name": "tag:Project", "Values": ["boot-race"]},
                                        {"Name": "instance-state-name", "Values": ["pending", "running", "stopping", "stopped"]}])
    ids = [i["InstanceId"] for res in r["Reservations"] for i in res["Instances"]]
    if ids:
        ec2.terminate_instances(InstanceIds=ids)
        ec2.get_waiter("instance_terminated").wait(InstanceIds=ids, WaiterConfig={"Delay": 10, "MaxAttempts": 90})
    race.log(f"terminated {len(ids)} instances")
    if cfg.get("ami"):
        snaps = [b["Ebs"]["SnapshotId"] for im in ec2.describe_images(ImageIds=[cfg["ami"]])["Images"]
                 for b in im["BlockDeviceMappings"] if "Ebs" in b]
        ec2.deregister_image(ImageId=cfg["ami"])
        for sid in snaps:
            ec2.delete_snapshot(SnapshotId=sid)
        race.log(f"deregistered the AMI and {len(snaps)} snapshots")
    ecr = s.client("ecr")
    imgs = [{"imageDigest": d["imageDigest"]} for d in ecr.describe_images(repositoryName="boot-race")["imageDetails"]]
    if imgs:
        r = ecr.batch_delete_image(repositoryName="boot-race", imageIds=imgs)
        if r.get("failures"):
            raise SystemExit(f"image delete failures: {r['failures']}")
    race.log(f"deleted {len(imgs)} image references from the boot-race repository")


def leftovers(cfg, s):
    """Fail if any Project=boot-race instance, image or snapshot, pushed image, or campaign task is still there."""
    ec2, ecs, ecr = s.client("ec2"), s.client("ecs"), s.client("ecr")
    tag = [{"Name": "tag:Project", "Values": ["boot-race"]}]
    left = []
    r = ec2.describe_instances(Filters=tag)
    left += [f"instance {i['State']['Name']}" for res in r["Reservations"] for i in res["Instances"]
             if i["State"]["Name"] != "terminated"]
    left += [f"image {im['Name']}" for im in ec2.describe_images(Owners=["self"], Filters=tag)["Images"]]
    left += ["snapshot" for _ in ec2.describe_snapshots(OwnerIds=["self"], Filters=tag)["Snapshots"]]
    left += ["pushed image" for _ in ecr.describe_images(repositoryName="boot-race")["imageDetails"]]
    path = cfg["results"] + "/tasks.txt"
    arns = sorted(set(open(path).read().split())) if os.path.exists(path) else []
    for i in range(0, len(arns), 100):
        for t in ecs.describe_tasks(cluster="boot-race", tasks=arns[i:i + 100])["tasks"]:
            if t["lastStatus"] != "STOPPED":
                left.append(f"task {t['lastStatus']}")
    race.log(f"checked {len(arns)} campaign tasks; left: {left or 'nothing'}")
    if left:
        raise SystemExit(f"{len(left)} campaign resources are still there: {left}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--config", required=True)
    ap.add_argument("step", choices=["image", "lambda", "ami", "instances", "ecs-host", "cleanup", "leftovers"])
    a = ap.parse_args()
    with open(a.config) as f:
        cfg = json.load(f)
    s = race.session()
    {"image": image, "lambda": lambda_code, "ami": ami, "instances": instances, "ecs-host": ecs_host,
     "cleanup": cleanup, "leftovers": leftovers}[a.step](cfg, s)
    save(a.config, cfg)


if __name__ == "__main__":
    main()
