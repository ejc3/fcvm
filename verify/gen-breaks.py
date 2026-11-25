#!/usr/bin/env python3
"""Write GateVerify/Breaks.lean from the output of `lake exe gate-breaks traces`.

    cd verify && lake exe gate-breaks traces > /tmp/traces.txt && ./gen-breaks.py /tmp/traces.txt

`items` below names the runs that are recorded: a rule book, a configuration and a property pick
the shortest run the search printed for them; `H` picks a run written by hand (`handRuns` in
GateSearch.lean). The design's theorem beside each run is read off the trace's `design-run:` and
`design:` lines. A run the trace does not have, or one it marks as not possible or not broken,
stops the script.
"""
import os,re,sys
t=open(sys.argv[1]).read()
blocks=re.split(r'\n(?=### |## )',t)
tr={}
def parse(b):
    lines=b.split('\n')
    term=[l for l in lines if l.startswith('  [')][0].strip()
    drun=[l for l in lines if l.startswith('  design-run: ')][0].strip()[len('design-run: '):]
    dver=[l for l in lines if l.startswith('  design: ')][0].strip()[len('design: '):]
    bad=[l for l in lines if l.startswith('  NOT ')]
    assert not bad, b[:200]
    f=lambda x:[y.strip() for y in x[1:-1].split(', ')] if x!='[]' else []
    return f(term), f(drun), dver
for b in blocks:
    m=re.match(r'### HAND (\S+) / (\S+) / (\S+): (.*?) \((\d+) steps\)',b)
    if m:
        tr[('HAND',m.group(1))]=parse(b); continue
    m=re.match(r'### (\S+) / (\S+): (.*?) \((\d+) steps\)',b)
    if not m: continue
    tr[(m.group(1),m.group(2),m.group(3))]=parse(b)
book={'ctstate':'Today.ctstate','flags':'Today.flags','cookieA':'Today.cookieA','cookieB':'Today.cookieB','cookieB-one-dump':'Today.cookieBOneLook','cookieB-restart-kept':'Today.cookieBKept'}
def ident(n): return re.sub(r'-(\w)', lambda m: m.group(1).upper(), n)
chk={'P1 sourceDisturbed':'sourceDisturbed','P2 generationBroken':'generationBroken','P3 staleAtPublication':'staleAtPublication','P3 smuggledLate':'smuggledLate','P4 workloadBehindEgress':'workloadBehindEgress','P5 lastingChange':'(lastingChange Runs.%s)','P5 capturedChange':'capturedChange','P6 stuck':'(stuck %s Runs.%s)','U1 clientDatagram':'clientDatagram','-- replyDatagram':'replyDatagram'}
H='HAND'
items=[
 ('Known breaks', None,None,None,None),
 ('liveReset','ctstate','routed','P1 sourceDisturbed',"**The second fault of #2106.** On `ctstate`, a guest that tracks nothing: closing the gate attaches the tracker, a segment of a connection that was already open is the first it sees of it, so it is NEW, and the gate answers a live client of the source with a reset"),
 ('staleTuple','ctstate','routed-stale','P3 staleAtPublication',"**#2106.** On `ctstate`, a SYN on a tuple the guest still tracks as ESTABLISHED is not NEW: it passes the source's closed gate after the dump, the request socket is in the image and in no manifest, and the clone publishes with it"),
 ('staleTupleLate','ctstate','routed-stale','P3 smuggledLate',"#2106 on the clone: the same SYN passes the clone's closed gate after its retirement"),
 ('staleTupleGrown','ctstate','routed','P3 staleAtPublication',"#2106 with nothing assumed, by the issue's own route: where the stale entry comes from. The gate's rule attaches IPv4 tracking during one snapshot and it stays attached a little after the gate is removed; a client's handshake is tracked then; the tracker is detached before the client closes, so the close is not seen; and the next snapshot's clone publishes with that client's new connection"),
 ('halfOpenRetransmit','ctstate','nat','P3 smuggledLate',"A variant of #2106 in a guest that always tracks. A request socket in the manifest is retired without a reset (`tcp_abort`, tcp.c:5062-5069), so its tracked entry stays; the client's SYN, sent again, is ignored by the tracker, is not NEW, and passes the clone's closed gate"),
 ('reusedTuple',H,'nat','P3 smuggledLate',"And with an established connection: the clone's retirement resets it from the guest's side, which leaves the tracked entry closed by a reset that was not the client's. A client that connects again from the same address and port within the entry's timeout (10 s in Linux 6.18.60) sends a SYN the tracker does not recycle: not NEW, it passes the clone's closed gate. `ctstate` only. Written by hand: the search reports the shorter `halfOpenRetransmit`"),
 ('cookieAck','flags','cookie','P3 staleAtPublication',"**#2123.** With the flag rule, the ACK that completes a SYN-cookie handshake passes the source's closed gate between its dump and its pause. The socket is in the image and in no manifest, and the clone publishes with it"),
 ('cookieAckClone','flags','cookie','P3 smuggledLate',"#2123 on the clone: a cookie's ACK makes a socket after the retirement, before publication"),
 ('loopbackClient','flags','loopback-port','P3 staleAtPublication',"**The `/loopback` port.** A connection from a client of a port published with `/loopback` records a loopback peer, so the manifest leaves it out as a connection inside the guest, and every clone publishes with it. The same on `ctstate`"),
 ('loopbackClientA','cookieA','loopback-port','P3 staleAtPublication',"The review of a80e1ad34, P2: its own list skips a loopback peer the same way, so such a socket, made by a cookie or not, is published"),
 ('twoDumps','cookieA','pairs','P2 generationBroken',"The review of a80e1ad34, the list is two dumps: a connection made inside the guest between the IPv4 dump and the IPv6 dump has one end listed, and the restore retires it. It needs a connection whose listed end does not record a loopback peer: a client bound to the guest's own address"),
 ('oneDump','cookieB-one-dump','pairs','P2 generationBroken',"What 27add63dd's second dump is for: without it, and with the mirror image deciding, any connection made inside the guest between the two dumps loses an end"),
 ('cookiesBackFirst','cookieA','cookie','P3 smuggledLate',"The review of a80e1ad34, publication's order: `tcp_syncookies` is written back before the ingress half is removed, so a cookie's ACK in between makes a socket in a clone that has not published (and one that never does, if the removal fails)"),
 ('proxyConnectA','cookieA','proxy','P2 generationBroken',"The review of a80e1ad34, the egress proxy: nat OUTPUT redirects a connect before the gate's OUTPUT chain sees it, so a process outside the frozen container connects out behind the closed egress half, and the restore's list retires its two sockets"),
 ('proxyConnect','cookieB','proxy','P2 generationBroken',"The same at 27add63dd, which documents it: both sockets are retired and the process connects again"),
 ('proxyStale','flags','proxy','P3 staleAtPublication',"The same connect on the source, where `ctstate` and #2113 have no list: made behind the source's closed egress half after the dump, its two sockets are in the image and in no manifest, and the clone publishes holding them. (The relay behind the proxy is vsock and the restore resets it by other means, which the model does not hold; the two TCP sockets are what the test reads.)"),
 ('hostDies','cookieB','host','P6 stuck',"**#1936.** The host's `snapshot create` dies after the prepare: nothing opens the source's gate"),
 ('Breaks the search found in PR #2124 at its head', None,None,None,None),
 ('earlySnapshot','cookieB','early','P3 staleAtPublication',"`snapshot create` of a clone whose restore has not taken the boundary lock yet: the prepare closes a gate that is already closed, and the resume one-shot opens it. The clone is open to clients with every socket of its source and no restore"),
 ('earlySnapshotFails','cookieB','early-fails','P3 staleAtPublication',"The same with a prepare that fails: its rollback opens the gate"),
 ('earlySnapshotCaptures','cookieB','early','P5 capturedChange',"A snapshot of a clone whose restore begins between the prepare and the pause: the image holds `tcp_syncookies` at 0"),
 ('failedCloneSnapshot',H,'early-fails','P5 capturedChange',"**Can a snapshot capture `tcp_syncookies` at 0?** Yes. A restore that fails after `refuse_syn_cookies` leaves the 0 and goes to `shutdown_vm`; a `snapshot create` asked of the clone before the power-off lands is not refused, and its image holds the 0. A clone of that image resumes in the same `shutdown_vm` and powers off (`failedCloneSnapshotEnds`)"),
 ('agentDies','cookieB','kills','P6 stuck',"fc-agent dies between the prepare and the reopen. The one-shots are exec'd through it, so the host's resume one-shot cannot run, `snapshot create` returns its error, and the source keeps what the prepare had installed. On Firecracker the restarted agent fetches its boot plan over MMDS, behind the closed egress half, and never gets it: the source stays gated for good. (Over vsock the restarted agent gets its plan, and today its start of the container then fails and it powers the VM off, #2158: `restartPowersOffToday` in `Ends.lean`.)"),
 ('agentDiesSource','cookieB','kills','P1 sourceDisturbed',"The same run read as P1: `snapshot create` is over and the source has a half of the gate"),
 ('Once #2158 is fixed and nothing else', None,None,None,None),
 ('agentDiesVsock','cookieB-restart-kept','kills-vsock','P6 stuck',"Not a break of the code today, where this run ends in the power-off. With #2158 fixed (a restarted fc-agent gets a container running again), over vsock (Cloud Hypervisor), in a guest that booted without the snapshot cache: the agent comes back with its exec server after the host's exec has given up, starts the container, and nothing asks for the reopen. (If it is back inside the exec's 54 s of connect retries, which a restart of about a second is, the reopen lands and nothing is broken.)"),
 ('containerMidBoundary','cookieB-restart-kept','kills-vsock','P4 workloadBehindEgress',"Not a break of the code today either. With #2158 fixed, over vsock: fc-agent dies after the prepare one-shot has replied and is back before the host's pause, which is the next call the host makes. The image holds the new agent with its container not yet started, and the clone's agent starts it during the restore, behind the closed egress half. The restart alone takes about a second (`RestartSec=1`), so the agent has to die and come back between two adjacent host calls"),
 ('Breaks the search found in PR #2124 at its head, continued', None,None,None,None),
 ('barrierKill',H,'kills','P1 sourceDisturbed',"The prepare one-shot is killed inside the receive barrier: `/sys/kernel/rcu_expedited` stays 1 on the source after the snapshot has failed and the gate is open again. Written by hand: in this configuration the search reports the shorter `agentDiesSource`"),
 ('barrierKillCaptured','cookieB','kills','P5 capturedChange',"And every later snapshot of that guest captures the 1: the next barrier finds the switch on and leaves it on"),
 ('watchdogThaw','cookieB','slow','P4 workloadBehindEgress',"A boundary held past the payload freeze's deadline: the watchdog thaws the container while both halves of the clone's gate are closed"),
 ('reopenFails','cookieB','fails','P1 sourceDisturbed',"The resume one-shot fails to open the gate. It removes the manifest, thaws, exits non-zero, and nothing tries again: the source stays gated"),
 ('reopenFailsStuck','cookieB','fails','P6 stuck',"The same run read as P6: the gate is closed and no step of the system is left that would open it"),
 ('writeBackFails','cookieB','fails','P5 lastingChange',"The write of `tcp_syncookies` back fails three times: the clone is published without SYN cookies, with a warning (documented in the PR)"),
 ('UDP', None,None,None,None),
 ('replyDatagram','cookieB','udp','-- replyDatagram',"What a stale entry opens: a datagram from the address and port the guest itself sent to before the snapshot is in the reply direction of an entry the clone inherits, and the UDP rule passes it. Not a break: the rule says so"),
 ('clientDatagram','cookieB','udp-reach','U1 clientDatagram',"Without the host's gate, a client's datagram reaches a clone between its resume and its UDP rule (#1114)"),
]
handbook={'reusedTuple':'ctstate','barrierKill':'cookieB','failedCloneSnapshot':'cookieB'}
out=['''import GateVerify.Properties
import GateVerify.Today
import GateVerify.Design
import GateVerify.Configs

/-!
# The rules as they are, broken: each run the search found, checked by the kernel

Written by `../gen-breaks.py` from the trace output; edit that, not this file.

Copied exactly from `lake exe gate-breaks traces`: for each kind of break, the shortest run from
`init` after which a property is broken, under the rule book and the configuration it was found
in, and a few runs written by hand where the search reports a break only through a shorter run
of another kind (`handRuns` in `GateSearch.lean`). `by decide` replays the run and evaluates the
property in the kernel, so each is a checked fact about the model: a run that stops being
possible, or stops breaking its property, fails the build. `BREAKS.md` has each in words.

Each is followed by what the design does with the same run, aligned to the design's own restore
order: where the run has the agent do a restore step, the design's agent does its steps up to
and including that one (`printRun` in `GateSearch.lean`). Either the design refuses a step of
it (`design_refuses_`: the aligned run is not possible, and the refused step is its last), or
the run is possible and the property holds at its end (`design_keeps_`). These say what the
design does with one run. That the design keeps a property on every run is `Proofs.lean`. The
two UDP runs are the exception: the design does not change them (`design_too_`).
-/

namespace Gate

''']
def wrap(prefix, items, width=100, indent='   '):
    lines=[]; cur=prefix
    for i,it in enumerate(items):
        piece=it+(', ' if i<len(items)-1 else ']')
        if len(cur)+len(piece.rstrip())>width:
            lines.append(cur.rstrip()); cur=indent+piece
        else: cur+=piece
    lines.append(cur.rstrip())
    return '\n'.join(lines)
def wrapdoc(s, width=98):
    words=s.split(' '); lines=[]; cur=''
    for w in words:
        if cur and len(cur)+1+len(w)>width: lines.append(cur); cur=w
        else: cur=(cur+' '+w) if cur else w
    lines.append(cur); return '\n'.join(lines)
names=[]; table=[]
for name,bk,run,check,doc in items:
    if bk is None:
        out.append(f'/-! ## {name} -/\n'); continue
    if bk==H:
        labs,drun,dver=tr[(H,name)]; bk=handbook[name]
    else:
        labs,drun,dver=tr[(bk,run,check)]
    n=len(labs)
    R=book[bk]; C='Runs.'+ident(run)
    c=chk[check]
    if check=='P5 lastingChange': c=c%ident(run)
    if check=='P6 stuck': cd=c%(R,ident(run)); cdes=c%('Design.rules',ident(run))
    else: cd=c; cdes=c
    out.append('/-- '+wrapdoc(f'{doc} ({n} steps).')+' -/')
    out.append(wrap(f'def Run.{name} : List Label :=\n  [', labs))
    out.append('')
    out.append(f'theorem breaks_{name} :\n    after (replay {R} {C} Run.{name} (init {C}))\n      {cd} := by decide\n')
    if drun==labs: dname=f'Run.{name}'
    else:
        dname=f'Run.{name}Design'
        out.append(f'/-- The same run with the design\'s restore: {"it refuses the last step" if dver.startswith("refuses") else "the same steps in its own order"}. -/')
        out.append(wrap(f'def {dname} : List Label :=\n  [', drun))
        out.append('')
    if dver.startswith('refuses'):
        k=int(dver.split()[-1]); assert k==len(drun)
        out.append(f'theorem design_refuses_{name} :\n    replay Design.rules {C} {dname} (init {C}) = none\n    ∧ (replay Design.rules {C} ({dname}.take {k-1}) (init {C})).isSome = true := by decide\n')
        table.append((name,bk,run,check,n,f'refuses step {k}: {drun[-1]}'))
    elif dver=='runs, holds':
        out.append(f'theorem design_keeps_{name} :\n    (replay Design.rules {C} {dname} (init {C})).map\n      {cdes} = some false := by decide\n')
        table.append((name,bk,run,check,n,'runs, holds'))
    else:
        assert name in ('replyDatagram','clientDatagram'), (name,dver)
        out.append(f'/-- The design does not change this. -/\ntheorem design_too_{name} :\n    after (replay Design.rules {C} {dname} (init {C}))\n      {cdes} := by decide\n')
        table.append((name,bk,run,check,n,'the same'))
    names.append(name)
    if name=='halfOpenRetransmit':
        out.append('''/-- #2113's flag rule refuses that SYN like any other. -/
theorem flags_keeps_halfOpenRetransmit :
    ¬ after (replay Today.flags Runs.nat Run.halfOpenRetransmit (init Runs.nat)) smuggledLate := by
  decide
''')
    if name=='reusedTuple':
        out.append('''/-- #2113's flag rule refuses that SYN too. -/
theorem flags_keeps_reusedTuple :
    ¬ after (replay Today.flags Runs.nat Run.reusedTuple (init Runs.nat)) smuggledLate := by
  decide
''')
    if name=='failedCloneSnapshot':
        out.append('''/-- The clone of that image: it is restored, and the only step its agent has left is the
power-off. -/
theorem failedCloneSnapshotEnds :
    (replay Today.cookieB { Runs.earlyFails with maxGens := 2 }
        (Run.failedCloneSnapshot ++ [.restore, .off]) (init Runs.earlyFails)).map (·.pw)
      = some false
    ∧ (replay Today.cookieB { Runs.earlyFails with maxGens := 2 }
        (Run.failedCloneSnapshot ++ [.restore, .agent]) (init Runs.earlyFails)) = none := by
  decide
''')
out.append('end Gate\n')
open(os.path.join(os.path.dirname(os.path.abspath(__file__)),'GateVerify','Breaks.lean'),'w').write('\n'.join(out))
for r in table: print('\t'.join(map(str,r)))
