import GateVerify.Socks

/-!
# The invariant over the generations: a retirement destroys nothing it must not

`Sk` (`Socks.lean`) shows a retirement destroys everything it must. `Gn` shows it destroys
nothing else: every cookie the manifest names is a socket from the image, every cookie the
restore's own list names is a socket that is not a process's own and has no mirror image left,
and a connection inside the guest that a process of the clone opened keeps both its ends. So the
record `bad` is never set.

`Evo2` adds to `Evo` what a step of the environment or of a process can do to connections
inside the guest: a new socket is nobody's mirror image unless both ends are new, a socket that
goes takes its mirror image with it, and a process connects out only with egress open.
-/

namespace Gate

set_option linter.unusedSimpArgs false
set_option linter.unusedVariables false

theorem hasMirror_iff {l : List Sock} {x : Sock} :
    hasMirror l x = true ↔ ∃ k, x.kind.mirror = some k ∧ ∃ y ∈ l, y.kind = k := by
  unfold hasMirror
  cases h : x.kind.mirror with
  | none => simp
  | some k => simp [List.any_eq_true, beq_iff_eq]

structure Gn (s : State) : Prop where
  /-- every cookie in the list and in the manifest has been handed out -/
  g1 : ∀ i ∈ s.lst, i < s.next
  g2 : ∀ i ∈ s.man.getD [], i < s.next
  /-- in a clone, a socket the manifest names is from the image -/
  gM : ∀ n, s.rp = .at n → ∀ x ∈ s.socks, x.id ∈ s.man.getD [] → x.gen = .stale
  /-- while a clone's ingress is closed, no packet from outside has made a socket it may keep -/
  gF : ∀ n, s.rp = .at n → n ≤ 15 → ∀ x ∈ s.socks, x.gen ≠ .fresh
  /-- while a clone's egress is closed, a process has opened only connections inside the guest -/
  gO : ∀ n, s.rp = .at n → n ≤ 10 → ∀ x ∈ s.socks, x.gen = .own → x.kind.mirror ≠ none
  /-- a connection inside the guest that a process opened has both its ends -/
  gP : ∀ x ∈ s.socks, x.gen = .own → ∀ k, x.kind.mirror = some k →
    ∃ y ∈ s.socks, y.kind = k ∧ y.gen = .own
  /-- after the IPv6 dump: every cookie in the list is a seen socket's -/
  gLS : s.rp = .at 9 → s.ag = true → ∀ i ∈ s.lst, ∃ y ∈ s.seen, y.id = i
  /-- after the second look: a listed socket is not a process's own, and has no mirror image -/
  gL : ∀ n, s.rp = .at n → 10 ≤ n → n ≤ 11 → s.ag = true → ∀ x ∈ s.socks, x.id ∈ s.lst →
    x.gen ≠ .own ∧ x.gen ≠ .fresh ∧ hasMirror s.socks x = false
  /-- no retirement has destroyed a socket it must not -/
  gB : s.bad = false

/-- What a step of the environment or of a process does to connections inside the guest. `S` are
the sockets before, `T` after, `next` the next cookie before, `out4` the egress half before. -/
structure Evo2 (S T : List Sock) (next : Nat) (out4 : Bool) : Prop where
  /-- a new socket is not the mirror image of an old one -/
  nm : ∀ z ∈ T, next ≤ z.id → ∀ x ∈ S, x.kind.mirror ≠ some z.kind
  /-- an old socket stays, or its kind is gone and so is every socket whose mirror image it was -/
  keep : ∀ x ∈ S, (∃ y ∈ T, y.id = x.id ∧ y.kind = x.kind ∧ y.gen = x.gen)
    ∨ (∀ y ∈ T, y.kind ≠ x.kind ∧ y.kind.mirror ≠ some x.kind)
  /-- a new end of a connection inside the guest comes with its other end -/
  np : ∀ z ∈ T, next ≤ z.id → z.gen = .own → ∀ k, z.kind.mirror = some k →
    ∃ y ∈ T, y.kind = k ∧ y.gen = .own
  /-- a process's new socket is an end of a connection inside the guest, or egress was open -/
  no : ∀ z ∈ T, next ≤ z.id → z.gen = .own → z.kind.mirror ≠ none ∨ out4 = false

theorem gn_evo {C : Cfg} {s t : State} (hs : Scal C s) (hpw : s.pw = true) (he : Evo s t)
    (he2 : Evo2 s.socks t.socks s.next s.out4) (hb : t.bad = s.bad) (hsc : t.scal = s.scal)
    (hg : Gn s) : Gn t := by
  simp only [State.scal, Prod.mk.injEq] at hsc
  obtain ⟨-, hrp, -, -, -, -, -, -, -, -, -, -, -, -, -, -, -, hag⟩ := hsc
  have hi := hs hpw
  have hnext := he.next_le
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro i hi'; rw [he.lst] at hi'; have := hg.g1 i hi'; omega
  · intro i hi'; rw [he.man] at hi'; have := hg.g2 i hi'; omega
  · intro n hn x hx hm
    rw [hrp] at hn; rw [he.man] at hm
    rcases he.old x hx with ⟨x0, hx0, h1, -, h3⟩ | ⟨h1, -, -⟩
    · rw [← h3]; exact hg.gM n hn x0 hx0 (by rw [h1]; exact hm)
    · have := hg.g2 x.id hm; omega
  · intro n hn h15 x hx
    rw [hrp] at hn
    rcases he.old x hx with ⟨x0, hx0, -, -, h3⟩ | ⟨-, -, h3⟩
    · rw [← h3]; exact hg.gF n hn h15 x0 hx0
    · rcases h3 with h3 | ⟨h3, -⟩
      · rw [h3]; simp
      · simp only [hn, CloneOk] at hi
        have hin : s.in4 = true ∧ s.in6 = true := hi.2.2.2.1 h15
        have : s.shutTo x.kind.fam = true := by
          cases hf : x.kind.fam <;> simp [State.shutTo, State.unpublished, hn, State.gateIn, hin]
        rw [h3]; simp [genIn, this]
  · intro n hn h10 x hx hown
    rw [hrp] at hn
    rcases he.old x hx with ⟨x0, hx0, -, h2, h3⟩ | ⟨h1, -, -⟩
    · rw [← h2]; exact hg.gO n hn h10 x0 hx0 (by rw [h3]; exact hown)
    · simp only [hn, CloneOk] at hi
      have hout : s.out4 = true ∧ s.out6 = true := hi.2.2.2.2.2.1 h10
      rcases he2.no x hx h1 hown with h | h
      · exact h
      · rw [hout.1] at h; cases h
  · intro x hx hown k hk
    rcases he.old x hx with ⟨x0, hx0, -, h2, h3⟩ | ⟨h1, -, -⟩
    · obtain ⟨y, hy, hyk, hyo⟩ := hg.gP x0 hx0 (by rw [h3]; exact hown) k (by rw [h2]; exact hk)
      rcases he2.keep y hy with ⟨y', hy', -, e2, e3⟩ | hgone
      · exact ⟨y', hy', by rw [e2]; exact hyk, by rw [e3]; exact hyo⟩
      · exact absurd (by rw [hk, hyk]) (hgone x hx).2
    · exact he2.np x hx h1 hown k hk
  · intro h9 ha i hi'
    rw [hrp] at h9; rw [hag] at ha; rw [he.lst] at hi'; rw [he.seen]
    exact hg.gLS h9 ha i hi'
  · intro n hn h10 h11 ha x hx hl
    rw [hrp] at hn; rw [hag] at ha; rw [he.lst] at hl
    rcases he.old x hx with ⟨x0, hx0, h1, h2, h3⟩ | ⟨h1, -, -⟩
    · obtain ⟨a, b, c⟩ := hg.gL n hn h10 h11 ha x0 hx0 (by rw [h1]; exact hl)
      refine ⟨by rw [← h3]; exact a, by rw [← h3]; exact b, ?_⟩
      cases hm : hasMirror t.socks x with
      | false => rfl
      | true =>
        exfalso
        obtain ⟨k, hk, z, hz, hzk⟩ := hasMirror_iff.mp hm
        rcases he.old z hz with ⟨z0, hz0, -, f2, -⟩ | ⟨f1, -, -⟩
        · have : hasMirror s.socks x0 = true :=
            hasMirror_iff.mpr ⟨k, by rw [h2]; exact hk, z0, hz0, by rw [f2]; exact hzk⟩
          rw [c] at this; cases this
        · exact he2.nm z hz f1 x0 hx0 (by rw [h2, hk, hzk])
    · have := hg.g1 x.id hl; omega
  · rw [hb]; exact hg.gB

/-! ## What each step of the environment does to connections inside the guest -/

theorem e2_same {S : List Sock} {next : Nat} {o : Bool} (hj : ∀ x ∈ S, x.id < next) :
    Evo2 S S next o := by
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro z hz hn; have := hj z hz; omega
  · intro x hx; exact Or.inl ⟨x, hx, rfl, rfl, rfl⟩
  · intro z hz hn; have := hj z hz; omega
  · intro z hz hn; have := hj z hz; omega

/-- A request socket becomes the established socket. -/
theorem e2_upgrade {S : List Sock} {next : Nat} {o : Bool} (k : Kind)
    (hj : ∀ x ∈ S, x.id < next) :
    Evo2 S (S.map (fun y => if y.kind == k then { y with st := St.est } else y)) next o := by
  have hid : ∀ x : Sock, (if x.kind == k then { x with st := St.est } else x).id = x.id := by
    intro x; split <;> rfl
  have hkind : ∀ x : Sock, (if x.kind == k then { x with st := St.est } else x).kind = x.kind := by
    intro x; split <;> rfl
  have hgen : ∀ x : Sock, (if x.kind == k then { x with st := St.est } else x).gen = x.gen := by
    intro x; split <;> rfl
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro z hz hn
    simp only [List.mem_map] at hz
    obtain ⟨x, hx, rfl⟩ := hz
    have := hj x hx; rw [hid] at hn; omega
  · intro x hx
    exact Or.inl ⟨_, List.mem_map.mpr ⟨x, hx, rfl⟩, hid x, hkind x, hgen x⟩
  · intro z hz hn
    simp only [List.mem_map] at hz
    obtain ⟨x, hx, rfl⟩ := hz
    have := hj x hx; rw [hid] at hn; omega
  · intro z hz hn
    simp only [List.mem_map] at hz
    obtain ⟨x, hx, rfl⟩ := hz
    have := hj x hx; rw [hid] at hn; omega

/-- Sockets of the kinds `ks` go, and no socket that stays had one of them as its mirror image. -/
theorem e2_sub {S T : List Sock} {next : Nat} {o : Bool} (ks : List Kind)
    (hT : ∀ y, y ∈ T ↔ y ∈ S ∧ y.kind ∉ ks)
    (hcl : ∀ k ∈ ks, ∀ k' : Kind, k'.mirror = some k → k' ∈ ks)
    (hj : ∀ x ∈ S, x.id < next) : Evo2 S T next o := by
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro z hz hn; have := hj z ((hT z).mp hz).1; omega
  · intro x hx
    by_cases hk : x.kind ∈ ks
    · refine Or.inr ?_
      intro y hy
      have hy' := (hT y).mp hy
      refine ⟨?_, ?_⟩
      · intro e; rw [e] at hy'; exact hy'.2 hk
      · intro e; exact hy'.2 (hcl x.kind hk y.kind e)
    · exact Or.inl ⟨x, (hT x).mpr ⟨hx, hk⟩, rfl, rfl, rfl⟩
  · intro z hz hn; have := hj z ((hT z).mp hz).1; omega
  · intro z hz hn; have := hj z ((hT z).mp hz).1; omega

/-- One new socket: made by a packet, or a process's own with egress open. -/
theorem e2_add1 {S : List Sock} {next : Nat} {o : Bool} (x : Sock) (hid : x.id = next)
    (hnm : ∀ y ∈ S, y.kind.mirror ≠ some x.kind)
    (hg : x.gen ≠ .own ∨ (x.kind.mirror = none ∧ o = false))
    (hj : ∀ y ∈ S, y.id < next) : Evo2 S (ins x S) next o := by
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro z hz hn
    rcases mem_ins.mp hz with rfl | hz
    · exact hnm
    · have := hj z hz; omega
  · intro y hy; exact Or.inl ⟨y, mem_ins.mpr (Or.inr hy), rfl, rfl, rfl⟩
  · intro z hz hn hown k hk
    rcases mem_ins.mp hz with rfl | hz
    · rcases hg with h | h
      · exact absurd hown h
      · rw [h.1] at hk; cases hk
    · have := hj z hz; omega
  · intro z hz hn hown
    rcases mem_ins.mp hz with rfl | hz
    · rcases hg with h | h
      · exact absurd hown h
      · exact Or.inr h.2
    · have := hj z hz; omega

/-- Two new sockets of a process: the two ends of a connection inside the guest, or the two
sockets of a connect through the proxy with egress open. -/
theorem e2_add2 {S : List Sock} {next : Nat} {o : Bool} (x1 x2 : Sock) (hid1 : x1.id = next)
    (hid2 : x2.id = next + 1)
    (hnm : ∀ y ∈ S, y.kind.mirror ≠ some x1.kind ∧ y.kind.mirror ≠ some x2.kind)
    (hown : x1.gen = .own ∧ x2.gen = .own)
    (hp : (x1.kind.mirror = some x2.kind ∧ x2.kind.mirror = some x1.kind)
      ∨ (x1.kind.mirror = none ∧ x2.kind.mirror = none ∧ o = false))
    (hj : ∀ y ∈ S, y.id < next) : Evo2 S (ins x2 (ins x1 S)) next o := by
  have hmem : ∀ z, z ∈ ins x2 (ins x1 S) ↔ z = x2 ∨ z = x1 ∨ z ∈ S := by
    intro z; rw [mem_ins, mem_ins]
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro z hz hn y hy
    rcases (hmem z).mp hz with rfl | rfl | hz
    · exact (hnm y hy).2
    · exact (hnm y hy).1
    · have := hj z hz; omega
  · intro y hy; exact Or.inl ⟨y, (hmem y).mpr (Or.inr (Or.inr hy)), rfl, rfl, rfl⟩
  · intro z hz hn _ k hk
    rcases (hmem z).mp hz with rfl | rfl | hz
    · rcases hp with h | h
      · rw [h.2] at hk; cases hk
        exact ⟨x1, (hmem x1).mpr (Or.inr (Or.inl rfl)), rfl, hown.1⟩
      · rw [h.2.1] at hk; cases hk
    · rcases hp with h | h
      · rw [h.1] at hk; cases hk
        exact ⟨x2, (hmem x2).mpr (Or.inl rfl), rfl, hown.2⟩
      · rw [h.1] at hk; cases hk
    · have := hj z hz; omega
  · intro z hz hn _
    rcases (hmem z).mp hz with rfl | rfl | hz
    · rcases hp with h | h
      · left; rw [h.2]; simp
      · exact Or.inr h.2.2
    · rcases hp with h | h
      · left; rw [h.1]; simp
      · exact Or.inr h.2.2
    · have := hj z hz; omega

/-! ### The sockets and the record `bad` through each update -/

@[simp] theorem ctSet_socks (s : State) (c : Cli) (p : Port) (v : Option CtSt) :
    (s.ctSet c p v).socks = s.socks := by
  cases c <;> cases p <;> rfl

@[simp] theorem ckSet_socks (s : State) (c : Cli) (p : Port) : (s.ckSet c p).socks = s.socks := by
  cases c <;> cases p <;> rfl

@[simp] theorem noteRst_socks (s : State) (b : Bool) : (s.noteRst b).socks = s.socks := rfl

@[simp] theorem noteLate_socks (s : State) (b : Bool) : (s.noteLate b).socks = s.socks := rfl

@[simp] theorem add_socks (s : State) (k : Kind) (st : St) (g : Gen) :
    (s.add k st g).socks = ins { kind := k, id := s.next, st := st, gen := g } s.socks := rfl

@[simp] theorem add_next (s : State) (k : Kind) (st : St) (g : Gen) :
    (s.add k st g).next = s.next + 1 := rfl

@[simp] theorem drop_socks (s : State) (k : Kind) :
    (s.drop k).socks = s.socks.filter (·.kind != k) := rfl

@[simp] theorem upgrade_socks (s : State) (k : Kind) :
    (s.upgrade k).socks
      = s.socks.map (fun y => if y.kind == k then { y with st := St.est } else y) := rfl

@[simp] theorem answered_socks (tr : Bool) (c : Cli) (p : Port) (seg : Seg) (r : Bool)
    (s : State) : (answered tr c p seg r s).socks = s.socks := by
  unfold answered
  split
  · rfl
  · split <;> simp

@[simp] theorem unanswered_socks (R : Rules) (C : Cfg) (c : Cli) (p : Port) (seg : Seg)
    (s : State) : (unanswered R C c p seg s).socks = s.socks := by
  unfold unanswered
  split <;> simp

@[simp] theorem rejected_socks (R : Rules) (C : Cfg) (c : Cli) (p : Port) (seg : Seg)
    (s : State) : (rejected R C c p seg s).socks = s.socks := by
  unfold rejected
  split <;> simp

@[simp] theorem ctSet_bad (s : State) (c : Cli) (p : Port) (v : Option CtSt) :
    (s.ctSet c p v).bad = s.bad := by
  cases c <;> cases p <;> rfl

@[simp] theorem ckSet_bad (s : State) (c : Cli) (p : Port) : (s.ckSet c p).bad = s.bad := by
  cases c <;> cases p <;> rfl

@[simp] theorem noteRst_bad (s : State) (b : Bool) : (s.noteRst b).bad = s.bad := rfl

@[simp] theorem noteLate_bad (s : State) (b : Bool) : (s.noteLate b).bad = s.bad := rfl

@[simp] theorem add_bad (s : State) (k : Kind) (st : St) (g : Gen) :
    (s.add k st g).bad = s.bad := rfl

@[simp] theorem drop_bad (s : State) (k : Kind) : (s.drop k).bad = s.bad := rfl

@[simp] theorem upgrade_bad (s : State) (k : Kind) : (s.upgrade k).bad = s.bad := rfl

@[simp] theorem answered_bad (tr : Bool) (c : Cli) (p : Port) (seg : Seg) (r : Bool)
    (s : State) : (answered tr c p seg r s).bad = s.bad := by
  unfold answered
  split
  · rfl
  · split <;> simp

@[simp] theorem unanswered_bad (R : Rules) (C : Cfg) (c : Cli) (p : Port) (seg : Seg)
    (s : State) : (unanswered R C c p seg s).bad = s.bad := by
  unfold unanswered
  split <;> simp

@[simp] theorem rejected_bad (R : Rules) (C : Cfg) (c : Cli) (p : Port) (seg : Seg)
    (s : State) : (rejected R C c p seg s).bad = s.bad := by
  unfold rejected
  split <;> simp

theorem mirror_ne_inb (k : Kind) (c : Cli) (p : Port) : k.mirror ≠ some (.inb c p) := by
  cases k <;> simp [Kind.mirror]

theorem genIn_ne_own (s : State) (f : Fam) : genIn s f ≠ .own := by
  unfold genIn; split <;> simp

theorem pkt_evo2 {C : Cfg} {c : Cli} {p : Port} {seg : Seg} {s t : State} :
    pkt Design.rules C c p seg s = some t → (∀ x ∈ s.socks, x.id < s.next) →
      Evo2 s.socks t.socks s.next s.out4 ∧ t.bad = s.bad := by
  unfold pkt
  repeat' split
  all_goals (intro h hj)
  all_goals (try contradiction)
  all_goals (simp only [Option.some.injEq] at h; subst h)
  all_goals (refine ⟨?_, by simp⟩)
  all_goals (simp only [answered_socks, unanswered_socks, rejected_socks, ckSet_socks,
    noteLate_socks, add_socks, drop_socks, upgrade_socks])
  all_goals first
    | exact e2_same hj
    | exact e2_upgrade _ hj
    | (exact e2_add1 _ rfl (fun y _ => mirror_ne_inb _ _ _) (Or.inl (genIn_ne_own _ _)) hj)
    | (refine e2_sub [.inb c p] ?_ ?_ hj
       · intro y; simp [List.mem_filter]
       · intro k hk k' hm
         simp only [List.mem_singleton] at hk
         subst hk
         exact absurd hm (mirror_ne_inb _ _ _))

theorem mirror_ne_outb (k : Kind) (w : Who) : k.mirror ≠ some (.outb w) := by
  cases k <;> simp [Kind.mirror]

theorem mirror_ne_proxC (k : Kind) (w : Who) : k.mirror ≠ some (.proxC w) := by
  cases k <;> simp [Kind.mirror]

theorem mirror_ne_proxS (k : Kind) (w : Who) : k.mirror ≠ some (.proxS w) := by
  cases k <;> simp [Kind.mirror]

theorem pairC_mirror (b : Bool) : (pairC b).mirror = some (pairS b) := by
  cases b <;> rfl

theorem pairS_mirror (b : Bool) : (pairS b).mirror = some (pairC b) := by
  cases b <;> rfl

theorem mirror_pairC {k : Kind} {b : Bool} (h : k.mirror = some (pairC b)) : k = pairS b := by
  cases b <;> cases k <;> simp_all [Kind.mirror, pairC, pairS]

theorem mirror_pairS {k : Kind} {b : Bool} (h : k.mirror = some (pairS b)) : k = pairC b := by
  cases b <;> cases k <;> simp_all [Kind.mirror, pairC, pairS]

theorem has_false {s : State} {k : Kind} (h : s.has k = false) : ∀ y ∈ s.socks, y.kind ≠ k := by
  intro y hy e
  have : s.has k = true := by
    simp only [State.has, List.any_eq_true, beq_iff_eq]
    exact ⟨y, hy, e⟩
  rw [h] at this; cases this

/-- Closes the lemma of a step that leaves the sockets and the record alone. -/
macro "evo2_same" : tactic => `(tactic| (
  repeat' split
  all_goals (intro h hj)
  all_goals (try contradiction)
  all_goals (simp only [Option.some.injEq] at h; subst h)
  all_goals (exact ⟨e2_same hj, rfl⟩)))

theorem udpC_evo2 {R : Rules} {C : Cfg} {s t : State} :
    udpC R C s = some t → (∀ x ∈ s.socks, x.id < s.next) →
      Evo2 s.socks t.socks s.next s.out4 ∧ t.bad = s.bad := by
  unfold udpC
  evo2_same

theorem udpR_evo2 {C : Cfg} {s t : State} :
    udpR C s = some t → (∀ x ∈ s.socks, x.id < s.next) →
      Evo2 s.socks t.socks s.next s.out4 ∧ t.bad = s.bad := by
  unfold udpR
  evo2_same

theorem udpS_evo2 {R : Rules} {C : Cfg} {s t : State} :
    udpS R C s = some t → (∀ x ∈ s.socks, x.id < s.next) →
      Evo2 s.socks t.socks s.next s.out4 ∧ t.bad = s.bad := by
  unfold udpS
  evo2_same

/-- In the design a process connects out only with the egress half open: the proxy's redirect
is closed with it. -/
theorem conn_evo2 {C : Cfg} {w : Who} {s t : State} :
    conn Design.rules C w s = some t → (∀ x ∈ s.socks, x.id < s.next) →
      Evo2 s.socks t.socks s.next s.out4 ∧ t.bad = s.bad := by
  unfold conn
  repeat' split
  all_goals (intro h hj)
  all_goals (try contradiction)
  all_goals (simp only [Option.some.injEq] at h; subst h)
  all_goals (refine ⟨?_, rfl⟩)
  · -- through the proxy
    rename_i hp _
    have hout : s.out4 = false := by simp [Design.rules] at hp; exact hp.2
    exact e2_add2 _ _ rfl rfl (fun y _ => ⟨mirror_ne_proxC _ _, mirror_ne_proxS _ _⟩) ⟨rfl, rfl⟩
      (Or.inr ⟨rfl, rfl, hout⟩) hj
  · rename_i hout _
    have hout : s.out4 = false := by simpa using hout
    exact e2_add1 _ rfl (fun y _ => mirror_ne_outb _ _) (Or.inr ⟨rfl, hout⟩) hj

theorem shut_evo2 {w : Who} {s t : State} :
    shut w s = some t → (∀ x ∈ s.socks, x.id < s.next) →
      Evo2 s.socks t.socks s.next s.out4 ∧ t.bad = s.bad := by
  unfold shut
  repeat' split
  all_goals (intro h hj)
  all_goals (try contradiction)
  all_goals (simp only [Option.some.injEq] at h; subst h)
  refine ⟨e2_sub [.outb w, .proxC w, .proxS w] ?_ ?_ hj, rfl⟩
  · intro y
    simp only [drop_socks, List.mem_filter, bne_iff_ne, ne_eq, List.mem_cons, List.mem_nil_iff,
      or_false, not_or]
    constructor
    · rintro ⟨⟨⟨h1, h2⟩, h3⟩, h4⟩; exact ⟨h1, h2, h3, h4⟩
    · rintro ⟨h1, h2, h3, h4⟩; exact ⟨⟨⟨h1, h2⟩, h3⟩, h4⟩
  · intro k hk k' hm
    simp only [List.mem_cons, List.mem_nil_iff, or_false] at hk
    rcases hk with rfl | rfl | rfl
    · exact absurd hm (mirror_ne_outb _ _)
    · exact absurd hm (mirror_ne_proxC _ _)
    · exact absurd hm (mirror_ne_proxS _ _)

theorem pair_evo2 {C : Cfg} {b : Bool} {s t : State} :
    pair C b s = some t → (∀ x ∈ s.socks, x.id < s.next) →
      Evo2 s.socks t.socks s.next s.out4 ∧ t.bad = s.bad := by
  unfold pair
  repeat' split
  all_goals (intro h hj)
  all_goals (try contradiction)
  all_goals (simp only [Option.some.injEq] at h; subst h)
  rename_i habs
  have hc : s.has (pairC b) = false := by
    cases hh : s.has (pairC b) <;> simp_all
  have hs : s.has (pairS b) = false := by
    cases hh : s.has (pairS b) <;> simp_all
  refine ⟨e2_add2 _ _ rfl rfl ?_ ⟨rfl, rfl⟩ (Or.inl ⟨pairC_mirror b, pairS_mirror b⟩) hj, rfl⟩
  intro y hy
  exact ⟨fun e => has_false hs y hy (mirror_pairC e), fun e => has_false hc y hy (mirror_pairS e)⟩

theorem unpair_evo2 {b : Bool} {s t : State} :
    unpair b s = some t → (∀ x ∈ s.socks, x.id < s.next) →
      Evo2 s.socks t.socks s.next s.out4 ∧ t.bad = s.bad := by
  unfold unpair
  repeat' split
  all_goals (intro h hj)
  all_goals (try contradiction)
  all_goals (simp only [Option.some.injEq] at h; subst h)
  refine ⟨e2_sub [pairC b, pairS b] ?_ ?_ hj, rfl⟩
  · intro y
    simp only [drop_socks, List.mem_filter, bne_iff_ne, ne_eq, List.mem_cons, List.mem_nil_iff,
      or_false, not_or]
    constructor
    · rintro ⟨⟨h1, h2⟩, h3⟩; exact ⟨h1, h2, h3⟩
    · rintro ⟨h1, h2, h3⟩; exact ⟨⟨h1, h2⟩, h3⟩
  · intro k hk k' hm
    simp only [List.mem_cons, List.mem_nil_iff, or_false] at hk ⊢
    rcases hk with rfl | rfl
    · exact Or.inr (mirror_pairC hm)
    · exact Or.inl (mirror_pairS hm)

/-! ## The system's own steps -/

/-- A step that leaves the sockets and the record alone, and either leaves the restore where it
was or moves it somewhere the phase-bound clauses follow from where it was. -/
theorem gn_sys {s t : State} (hg : Gn s) (h1 : t.socks = s.socks) (h2 : t.next = s.next)
    (hb : t.bad = s.bad)
    (hlst : ∀ i ∈ t.lst, i ∈ s.lst)
    (hman : ∀ i ∈ t.man.getD [], i < s.next)
    (hM : ∀ n, t.rp = .at n → ∃ m, s.rp = .at m ∧ ∀ i ∈ t.man.getD [], i ∈ s.man.getD [])
    (hF : ∀ n, t.rp = .at n → n ≤ 15 → ∃ m, s.rp = .at m ∧ m ≤ 15)
    (hO : ∀ n, t.rp = .at n → n ≤ 10 → ∃ m, s.rp = .at m ∧ m ≤ 10)
    (hLS : t.rp = .at 9 → t.ag = true →
      s.rp = .at 9 ∧ s.ag = true ∧ t.lst = s.lst ∧ t.seen = s.seen)
    (hL : ∀ n, t.rp = .at n → 10 ≤ n → n ≤ 11 → t.ag = true →
      ∃ m, s.rp = .at m ∧ 10 ≤ m ∧ m ≤ 11 ∧ s.ag = true ∧ t.lst = s.lst) : Gn t := by
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro i hi; rw [h2]; exact hg.g1 i (hlst i hi)
  · intro i hi; rw [h2]; exact hman i hi
  · intro n hn x hx hm
    obtain ⟨m, e1, e2⟩ := hM n hn
    rw [h1] at hx
    exact hg.gM m e1 x hx (e2 _ hm)
  · intro n hn h15 x hx
    obtain ⟨m, e1, e2⟩ := hF n hn h15
    rw [h1] at hx
    exact hg.gF m e1 e2 x hx
  · intro n hn h10 x hx
    obtain ⟨m, e1, e2⟩ := hO n hn h10
    rw [h1] at hx
    exact hg.gO m e1 e2 x hx
  · intro x hx hown k hk
    rw [h1] at hx ⊢
    exact hg.gP x hx hown k hk
  · intro h9 ha i hi
    obtain ⟨e1, e2, e3, e4⟩ := hLS h9 ha
    rw [e3] at hi; rw [e4]
    exact hg.gLS e1 e2 i hi
  · intro n hn h10 h11 ha x hx hl
    obtain ⟨m, e1, e2, e3, e4, e5⟩ := hL n hn h10 h11 ha
    rw [h1] at hx ⊢; rw [e5] at hl
    exact hg.gL m e1 e2 e3 e4 x hx hl
  · rw [hb]; exact hg.gB

theorem hasMirror_kind {l : List Sock} {x y : Sock} (h : x.kind = y.kind) :
    hasMirror l x = hasMirror l y := by
  unfold hasMirror; rw [h]

theorem hasMirror_sub {l l' : List Sock} {x : Sock} (hs : ∀ y ∈ l', y ∈ l)
    (h : hasMirror l x = false) : hasMirror l' x = false := by
  cases hm : hasMirror l' x with
  | false => rfl
  | true =>
    obtain ⟨k, hk, y, hy, hyk⟩ := hasMirror_iff.mp hm
    have : hasMirror l x = true := hasMirror_iff.mpr ⟨k, hk, y, hs y hy, hyk⟩
    rw [h] at this; cases this

theorem listIds_seen {man : List Nat} {seen : List Sock} :
    ∀ i ∈ listIds Design.rules man seen, ∃ y ∈ seen, y.id = i := by
  intro i hi
  simp only [listIds, Design.rules, List.mem_map, List.mem_filter] at hi
  obtain ⟨y, ⟨hy, -⟩, rfl⟩ := hi
  exact ⟨y, hy, rfl⟩

theorem manifestIds_lt {R : Rules} {seen now : List Sock} {n : Nat}
    (h : ∀ y ∈ seen, y.id < n) : ∀ i ∈ manifestIds R seen now, i < n := by
  intro i hi
  simp only [manifestIds, List.mem_map, List.mem_filter] at hi
  obtain ⟨y, ⟨hy, -⟩, rfl⟩ := hi
  exact h y hy

/-- A clone is restored: every socket is now the source's. -/
theorem restore_gn {C : Cfg} {s t : State} : restore C s = some t → Gn s → Gn t := by
  unfold restore
  split
  · intro h hg
    simp only [Option.some.injEq] at h
    subst h
    refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
    · intro i hi; simp at hi
    · exact hg.g2
    · intro n _ x hx _
      simp only [List.mem_map] at hx
      obtain ⟨x0, -, rfl⟩ := hx
      rfl
    · intro n _ _ x hx
      simp only [List.mem_map] at hx
      obtain ⟨x0, -, rfl⟩ := hx
      simp
    · intro n _ _ x hx hown
      simp only [List.mem_map] at hx
      obtain ⟨x0, -, rfl⟩ := hx
      simp at hown
    · intro x hx hown
      simp only [List.mem_map] at hx
      obtain ⟨x0, -, rfl⟩ := hx
      simp at hown
    · intro _ _ i hi; simp at hi
    · intro n _ _ _ _ x _ hl; simp at hl
    · exact hg.gB
  · intro h; contradiction

/-- The IPv6 dump: the list's cookies are seen sockets'. -/
theorem gn_dump6 {s t : State} (hk : Sk s) (hg : Gn s) (hs : s.rp = .at 8)
    (h1 : t.socks = s.socks) (h2 : t.next = s.next) (hb : t.bad = s.bad)
    (hseen : t.seen = s.seen ++ s.socks.filter (·.kind.fam == .v6))
    (hlst : t.lst = listIds Design.rules (s.man.getD []) t.seen) (hman : t.man = s.man)
    (hrp : t.rp = .at 9) : Gn t := by
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro i hi
    rw [hlst] at hi
    obtain ⟨y, hy, rfl⟩ := listIds_seen i hi
    rw [hseen, List.mem_append, List.mem_filter] at hy
    rw [h2]
    rcases hy with hy | hy
    · exact hk.j2 y hy
    · exact hk.j1 y hy.1
  · intro i hi; rw [hman] at hi; rw [h2]; exact hg.g2 i hi
  · intro n _ x hx hm
    rw [h1] at hx; rw [hman] at hm
    exact hg.gM 8 hs x hx hm
  · intro n _ _ x hx
    rw [h1] at hx
    exact hg.gF 8 hs (by omega) x hx
  · intro n _ _ x hx
    rw [h1] at hx
    exact hg.gO 8 hs (by omega) x hx
  · intro x hx hown k hkk
    rw [h1] at hx ⊢
    exact hg.gP x hx hown k hkk
  · intro _ _ i hi
    rw [hlst] at hi
    exact listIds_seen i hi
  · intro n hn h10 _
    rw [hrp] at hn; cases hn; omega
  · rw [hb]; exact hg.gB

/-- The second look: what stays on the list is no process's own socket and has no mirror image. -/
theorem gn_look {s t : State} (hk : Sk s) (hg : Gn s) (hs : s.rp = .at 9) (hag : s.ag = true)
    (h1 : t.socks = s.socks) (h2 : t.next = s.next) (hb : t.bad = s.bad)
    (hlst : t.lst = lookIds s.lst s.seen s.socks) (hman : t.man = s.man)
    (hrp : t.rp = .at 10) : Gn t := by
  have hsub : ∀ i ∈ t.lst, i ∈ s.lst := by
    intro i hi
    rw [hlst] at hi
    simp only [lookIds, List.mem_filter] at hi
    exact hi.1
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro i hi; rw [h2]; exact hg.g1 i (hsub i hi)
  · intro i hi; rw [hman] at hi; rw [h2]; exact hg.g2 i hi
  · intro n _ x hx hm
    rw [h1] at hx; rw [hman] at hm
    exact hg.gM 9 hs x hx hm
  · intro n _ _ x hx
    rw [h1] at hx
    exact hg.gF 9 hs (by omega) x hx
  · intro n _ _ x hx
    rw [h1] at hx
    exact hg.gO 9 hs (by omega) x hx
  · intro x hx hown k hkk
    rw [h1] at hx ⊢
    exact hg.gP x hx hown k hkk
  · intro h9; rw [hrp] at h9; cases h9
  · intro n _ _ _ _ x hx hl
    rw [h1] at hx ⊢
    rw [hlst] at hl
    simp only [lookIds, List.mem_filter, Bool.not_eq_true', List.any_eq_false] at hl
    obtain ⟨hl, hno⟩ := hl
    -- the listed cookie is a seen socket's, of this socket's kind, with no mirror image now
    obtain ⟨y, hy, hid⟩ := hg.gLS hs hag x.id hl
    have hkind : y.kind = x.kind := hk.u y hy x hx hid
    have hmir : hasMirror s.socks x = false := by
      have := hno y hy
      simp only [hid, beq_self_eq_true, Bool.true_and, Bool.not_eq_true] at this
      rw [← hasMirror_kind hkind]; exact this
    refine ⟨?_, hg.gF 9 hs (by omega) x hx, hmir⟩
    intro hown
    have hm := hg.gO 9 hs (by omega) x hx hown
    cases hk' : x.kind.mirror with
    | none => exact hm hk'
    | some k =>
      obtain ⟨z, hz, hzk, -⟩ := hg.gP x hx hown k hk'
      have : hasMirror s.socks x = true := hasMirror_iff.mpr ⟨k, hk', z, hz, hzk⟩
      rw [hmir] at this; cases this
  · rw [hb]; exact hg.gB

/-- The retirement: what it destroys is from the image or was made behind the closed gate, and
no connection a process opened inside the guest loses an end. The record stays clear. -/
theorem gn_retire {s t : State} (hg : Gn s) (hs : s.rp = .at 11) (hag : s.ag = true)
    (h1 : t.socks = s.socks.filter (fun x => !(s.man.getD [] ++ s.lst).contains x.id))
    (h2 : t.next = s.next)
    (hb : t.bad = (s.bad || (s.socks.filter (fun x => (s.man.getD [] ++ s.lst).contains x.id)).any
      (fun x => x.gen == .own || x.gen == .fresh
        || (!(s.man.getD []).contains x.id
            && hasMirror (s.socks.filter (fun x => !(s.man.getD [] ++ s.lst).contains x.id)) x))))
    (hlst : t.lst = []) (hman : t.man = s.man) (hrp : t.rp = .at 12) : Gn t := by
  have hsub : ∀ x ∈ t.socks, x ∈ s.socks := by
    intro x hx; rw [h1, List.mem_filter] at hx; exact hx.1
  -- a socket the retirement names is not a process's own
  have hnamed : ∀ x ∈ s.socks, x.id ∈ s.man.getD [] ∨ x.id ∈ s.lst → x.gen ≠ .own := by
    intro x hx hin
    rcases hin with hm | hl
    · rw [hg.gM 11 hs x hx hm]; simp
    · exact (hg.gL 11 hs (by omega) (by omega) hag x hx hl).1
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro i hi; rw [hlst] at hi; simp at hi
  · intro i hi; rw [hman] at hi; rw [h2]; exact hg.g2 i hi
  · intro n _ x hx hm
    rw [hman] at hm
    exact hg.gM 11 hs x (hsub x hx) hm
  · intro n _ _ x hx
    exact hg.gF 11 hs (by omega) x (hsub x hx)
  · intro n hn h10
    rw [hrp] at hn; cases hn; omega
  · intro x hx hown k hkk
    obtain ⟨y, hy, hyk, hyo⟩ := hg.gP x (hsub x hx) hown k hkk
    refine ⟨y, ?_, hyk, hyo⟩
    rw [h1, List.mem_filter]
    refine ⟨hy, ?_⟩
    simp only [Bool.not_eq_true', List.contains_eq_mem, List.mem_append, decide_eq_false_iff_not]
    intro hin
    exact hnamed y hy hin hyo
  · intro h9; rw [hrp] at h9; cases h9
  · intro n hn h10 h11
    rw [hrp] at hn; cases hn; omega
  · rw [hb, hg.gB, Bool.false_or, List.any_eq_false]
    intro x hx
    rw [List.mem_filter] at hx
    obtain ⟨hx, hc⟩ := hx
    simp only [List.contains_eq_mem, List.mem_append, decide_eq_true_eq] at hc
    have e1 : (x.gen == Gen.own) = false := by simpa using hnamed x hx hc
    by_cases hm : x.id ∈ s.man.getD []
    · have e2 : (x.gen == Gen.fresh) = false := by rw [hg.gM 11 hs x hx hm]; rfl
      simp [e1, e2, hm]
    · have hl : x.id ∈ s.lst := by
        rcases hc with h | h
        · exact absurd h hm
        · exact h
      obtain ⟨-, b, c⟩ := hg.gL 11 hs (by omega) (by omega) hag x hx hl
      have e2 : (x.gen == Gen.fresh) = false := by simpa using b
      have e3 : hasMirror (s.socks.filter
          (fun x => !(s.man.getD [] ++ s.lst).contains x.id)) x = false :=
        hasMirror_sub (fun y hy => (List.mem_filter.mp hy).1) c
      rw [e1, e2, e3]
      simp

theorem gn_dead {s : State} (hg : Gn s) : Gn s.dead := by
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩ <;> simp [State.dead, hg.gB]

set_option hygiene false in
/-- A side goal that holds because the state it speaks of cannot be: a restore is under way and
a snapshot is too, or the like. -/
macro "scal_absurd" : tactic => `(tactic| (
  have hi' := hi (by simp_all [State.running])
  first
    | (simp_all [CloneOk, FailedOk, SrcOk, State.unpublished, openGate, State.closeGate,
        State.dead]; done)
    | (simp_all [CloneOk, FailedOk, SrcOk, State.unpublished, openGate, State.closeGate,
        State.dead]; omega)))

set_option hygiene false in
macro "open_gn" : tactic => `(tactic| (
  repeat' split
  all_goals (intro h hi hk hg)
  all_goals (try contradiction)
  all_goals (try simp only [Option.ite_none_right_eq_some, Option.ite_none_left_eq_some,
    Option.some.injEq, reduceCtorEq] at h)
  all_goals (try contradiction)
  all_goals (repeat (obtain ⟨_, h⟩ := h))
  all_goals (try subst h)))

set_option hygiene false in
macro "close_gn" : tactic => `(tactic| (
  refine gn_sys hg rfl rfl rfl ?_ ?_ ?_ ?_ ?_ ?_ ?_
  · intro i hi0
    first
      | exact hi0
      | (simp at hi0; done)
  · intro i hi0
    first
      | exact hg.g2 i hi0
      | (simp at hi0; done)
      | exact manifestIds_lt hk.j2 i (by simpa using hi0)
  · intro n hr
    first
      | exact ⟨n, hr, fun i h0 => h0⟩
      | scal_absurd
  · intro n hr hle
    first
      | exact ⟨n, hr, hle⟩
      | scal_absurd
  · intro n hr hle
    first
      | exact ⟨n, hr, hle⟩
      | scal_absurd
  · intro hr ha
    first
      | exact ⟨hr, ha, rfl, rfl⟩
      | (simp at ha; done)
      | scal_absurd
  · intro n hr h10 h11 ha
    first
      | exact ⟨n, hr, h10, h11, ha, rfl⟩
      | (simp at ha; done)
      | scal_absurd))

theorem snap_gn {C : Cfg} {g : Bool} {s t : State} :
    snap Design.rules C g s = some t → Scal C s → Sk s → Gn s → Gn t := by
  unfold snap
  open_gn
  all_goals close_gn

theorem snapFail_gn {C : Cfg} {s t : State} :
    snapFail Design.rules C s = some t → Scal C s → Sk s → Gn s → Gn t := by
  unfold snapFail
  open_gn
  all_goals close_gn

theorem snapKill_gn {C : Cfg} {s t : State} :
    snapKill C s = some t → Scal C s → Sk s → Gn s → Gn t := by
  unfold snapKill
  open_gn
  all_goals close_gn

theorem kill_gn {C : Cfg} {s t : State} :
    kill C s = some t → Scal C s → Sk s → Gn s → Gn t := by
  unfold kill
  open_gn
  all_goals close_gn

theorem restart_gn {C : Cfg} {s t : State} :
    restart Design.rules C s = some t → Scal C s → Sk s → Gn s → Gn t := by
  unfold restart
  open_gn
  all_goals close_gn

theorem watchdog_gn {C : Cfg} {s t : State} :
    watchdog Design.rules C s = some t → Scal C s → Sk s → Gn s → Gn t := by
  unfold watchdog
  open_gn
  all_goals close_gn

theorem agent_gn {C : Cfg} {s t : State} (h : agent Design.rules C s = some t)
    (hi : Scal C s) (hk : Sk s) (hg : Gn s) : Gn t := by
  have hpw := agent_pw h
  obtain ⟨n, r, hr, hnr, hag, rfl⟩ := agent_spec h
  have hi' := hi hpw
  simp only [hr] at hi'
  have hn : n < 18 := hi'.1
  rcases lt18 hn with h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0
    | h0 | h0 | h0 | h0 <;> subst h0 <;> simp [Design.rules] at hnr <;> subst hnr
  all_goals first
    | -- a step that leaves the sockets, the lists and the record alone
      (refine gn_sys hg ?_ ?_ ?_ ?_ ?_ ?_ ?_ ?_ ?_ ?_
       · simp [advance, doStep, hr, Design.rules, openGate, State.closeGate]
       · simp [advance, doStep, hr, Design.rules, openGate, State.closeGate]
       · simp [advance, doStep, hr, Design.rules, openGate, State.closeGate]
       · intro i hi0
         simp [advance, doStep, hr, Design.rules, openGate, State.closeGate] at hi0 <;> exact hi0
       · intro i hi0
         simp [advance, doStep, hr, Design.rules, openGate, State.closeGate] at hi0 <;>
           exact hg.g2 i hi0
       · intro m hm
         refine ⟨_, hr, ?_⟩
         intro i hi0
         simp [advance, doStep, hr, Design.rules, openGate, State.closeGate] at hi0 <;> exact hi0
       · intro m hm hle
         simp [advance, doStep, hr, Design.rules, openGate, State.closeGate] at hm <;>
           (refine ⟨_, hr, ?_⟩; omega)
       · intro m hm hle
         simp [advance, doStep, hr, Design.rules, openGate, State.closeGate] at hm <;>
           (refine ⟨_, hr, ?_⟩; omega)
       · intro h9 ha
         simp [advance, doStep, hr, Design.rules, openGate, State.closeGate] at h9
       · intro m hm h10 h11 ha
         simp [advance, doStep, hr, Design.rules, openGate, State.closeGate] at hm <;>
           first
             | omega
             | (refine ⟨_, hr, ?_, ?_, hag, ?_⟩ <;>
                 first | omega | simp [advance, doStep, hr, Design.rules])
       done)
    | skip
  · -- load: with no manifest the restore fails; with one, nothing changes
    cases hm : s.man.isNone <;>
      (refine gn_sys hg ?_ ?_ ?_ ?_ ?_ ?_ ?_ ?_ ?_ ?_ <;>
        simp [advance, doStep, hr, Design.rules, hm]) <;>
      first
        | (intro i hi0; exact hg.g2 i hi0)
        | (intro i hi0; exact hi0)
        | (intro m hm' h9; omega)
  · refine gn_dump6 hk hg hr ?_ ?_ ?_ ?_ ?_ ?_ ?_ <;> simp [advance, doStep, hr, Design.rules]
  · refine gn_look hk hg hr hag ?_ ?_ ?_ ?_ ?_ ?_ <;> simp [advance, doStep, hr, Design.rules]
  · refine gn_retire hg hr hag ?_ ?_ ?_ ?_ ?_ ?_ <;> simp [advance, doStep, hr, Design.rules]

theorem agentFail_gn {C : Cfg} {s t : State} :
    agentFail Design.rules C s = some t → Gn s → Gn t := by
  unfold agentFail
  repeat' split
  all_goals (intro h hg)
  all_goals (try contradiction)
  all_goals first
    | (exfalso; simp_all; done)
    | (simp only [Option.some.injEq] at h
       subst h
       refine gn_sys hg rfl rfl rfl ?_ hg.g2 ?_ ?_ ?_ ?_ ?_ <;> simp)

theorem off_gn {s t : State} : off s = some t → Gn s → Gn t := by
  unfold off
  split
  · intro h hg; simp only [Option.some.injEq] at h; subst h; exact gn_dead hg
  · intro h; contradiction

theorem timeout_gn {s t : State} : timeout s = some t → Gn s → Gn t := by
  unfold timeout
  split
  · intro h hg; simp only [Option.some.injEq] at h; subst h; exact gn_dead hg
  · intro h; contradiction

/-- Every step of the design keeps `Gn`. -/
theorem step_gn {C : Cfg} {l : Label} {s t : State}
    (h : step Design.rules C l s = some t) (hi : Scal C s) (hk : Sk s) (hg : Gn s) : Gn t := by
  have hpw := step_pw h
  cases l with
  | pkt c p seg =>
    have e := pkt_evo2 h hk.j1
    exact gn_evo hi hpw (pkt_evo h hk.j1) e.1 e.2 (pkt_scal h) hg
  | udpC =>
    have e := udpC_evo2 h hk.j1
    exact gn_evo hi hpw (udpC_evo h hk.j1) e.1 e.2 (udpC_scal h) hg
  | udpR =>
    have e := udpR_evo2 h hk.j1
    exact gn_evo hi hpw (udpR_evo h hk.j1) e.1 e.2 (udpR_scal h) hg
  | udpS =>
    have e := udpS_evo2 h hk.j1
    exact gn_evo hi hpw (udpS_evo h hk.j1) e.1 e.2 (udpS_scal h) hg
  | detach =>
    simp only [step] at h
    split at h
    · simp only [Option.some.injEq] at h
      subst h
      exact gn_evo hi hpw (evo_same rfl hk.j1) (e2_same hk.j1) rfl rfl hg
    · contradiction
  | conn w =>
    have e := conn_evo2 h hk.j1
    exact gn_evo hi hpw (conn_evo h hk.j1) e.1 e.2 (conn_scal h) hg
  | shut w =>
    have e := shut_evo2 h hk.j1
    exact gn_evo hi hpw (shut_evo h hk.j1) e.1 e.2 (shut_scal h) hg
  | pair b =>
    have e := pair_evo2 h hk.j1
    exact gn_evo hi hpw (pair_evo h hk.j1) e.1 e.2 (pair_scal h) hg
  | unpair b =>
    have e := unpair_evo2 h hk.j1
    exact gn_evo hi hpw (unpair_evo h hk.j1) e.1 e.2 (unpair_scal h) hg
  | snap => exact snap_gn h hi hk hg
  | snapFail => exact snapFail_gn h hi hk hg
  | snapKill => exact snapKill_gn h hi hk hg
  | hostDie =>
    simp only [step] at h
    split at h
    · simp only [Option.some.injEq] at h
      subst h
      exact gn_evo hi hpw (evo_same rfl hk.j1) (e2_same hk.j1) rfl rfl hg
    · contradiction
  | guard => exact snap_gn h hi hk hg
  | restore => exact restore_gn h hg
  | agent => exact agent_gn h hi hk hg
  | agentFail => exact agentFail_gn h hg
  | kill => exact kill_gn h hi hk hg
  | restart => exact restart_gn h hi hk hg
  | off => exact off_gn h hg
  | timeout => exact timeout_gn h hg
  | watchdog => exact watchdog_gn h hi hk hg

theorem gn_init (C : Cfg) : Gn (init C) := by
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩ <;> simp [init]

theorem reachable_gn {C : Cfg} {s : State} (hr : Reachable Design.rules C s) : Gn s := by
  induction hr with
  | init => exact gn_init C
  | step l hs h ih => exact step_gn h (reachable_scal hs) (reachable_sk hs) ih

end Gate
