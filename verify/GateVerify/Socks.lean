import GateVerify.Lemmas

/-!
# The invariant over the sockets: nothing stale or smuggled is left at publication

`Sk` says, for the design, where every socket with its other end outside the guest is accounted
for while a restore runs: among the sockets a dump has seen, then in the manifest or the
restore's list, then gone. `Evo` is what one step of the environment or of a process can do to
the socket list, and `sk_evo` shows that any such step keeps `Sk`. The restore's own steps (the
dumps, the second look, the retirement) are shown one by one.
-/

namespace Gate

set_option linter.unusedSimpArgs false
set_option linter.unusedVariables false

/-- In the image, or made by a packet from outside behind a clone's closed ingress half. -/
def badGen (x : Sock) : Prop := x.gen = .stale ∨ x.gen = .smuggled

theorem mem_ins {x y : Sock} {l : List Sock} : y ∈ ins x l ↔ y = x ∨ y ∈ l := by
  induction l with
  | nil => simp [ins]
  | cons z l ih =>
    simp only [ins]
    split
    · simp
    · simp [ih]
      constructor
      · rintro (h | h | h)
        · exact Or.inr (Or.inl h)
        · exact Or.inl h
        · exact Or.inr (Or.inr h)
      · rintro (h | h | h)
        · exact Or.inr (Or.inl h)
        · exact Or.inl h
        · exact Or.inr (Or.inr h)

structure Sk (s : State) : Prop where
  /-- every cookie handed out is below the next one -/
  j1 : ∀ x ∈ s.socks, x.id < s.next
  j2 : ∀ y ∈ s.seen, y.id < s.next
  /-- one cookie, one kind -/
  w : ∀ x ∈ s.socks, ∀ y ∈ s.socks, x.id = y.id → x.kind = y.kind
  u : ∀ y ∈ s.seen, ∀ x ∈ s.socks, y.id = x.id → y.kind = x.kind
  /-- after the IPv4 dump: every IPv4 socket that must go has been seen -/
  k8 : s.rp = .at 8 → s.ag = true → ∀ x ∈ s.socks, x.kind.outside = true → x.kind.fam = .v4 →
    badGen x → ∃ y ∈ s.seen, y.id = x.id
  /-- after the IPv6 dump: every socket that must go is in the manifest or the list -/
  k5 : ∀ n, s.rp = .at n → 9 ≤ n → n ≤ 11 → s.ag = true → ∀ x ∈ s.socks,
    x.kind.outside = true → badGen x → x.id ∈ s.man.getD [] ∨ x.id ∈ s.lst
  /-- after the retirement: none is left -/
  k3 : s.cut = true → ∀ x ∈ s.socks, x.kind.outside = true → ¬ badGen x

/-- What a step of the environment or of a process of the guest does to the sockets: each
socket afterwards is one from before (its cookie, kind and origin unchanged) or has a new
cookie; a new one was opened by a process, or was made by a packet that found the ingress half
out for its family or `tcp_syncookies` on. -/
structure Evo (s t : State) : Prop where
  next_le : s.next ≤ t.next
  old : ∀ y ∈ t.socks,
    (∃ x ∈ s.socks, x.id = y.id ∧ x.kind = y.kind ∧ x.gen = y.gen)
    ∨ (s.next ≤ y.id ∧ y.id < t.next
        ∧ (y.gen = .own
            ∨ (y.gen = genIn s y.kind.fam ∧ (s.gateIn y.kind.fam = false ∨ s.sc = true))))
  inj : ∀ y ∈ t.socks, ∀ z ∈ t.socks, s.next ≤ y.id → y.id = z.id → y.kind = z.kind
  seen : t.seen = s.seen
  lst : t.lst = s.lst
  man : t.man = s.man

theorem sk_evo {C : Cfg} {s t : State} (hs : Scal C s) (hpw : s.pw = true) (he : Evo s t)
    (hsc : t.scal = s.scal) (hk : Sk s) : Sk t := by
  simp only [State.scal, Prod.mk.injEq] at hsc
  obtain ⟨-, hrp, -, -, -, -, -, -, -, -, -, -, -, -, -, hcut, -, hag⟩ := hsc
  have hi := hs hpw
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro y hy
    rcases he.old y hy with ⟨x, hx, h1, -, -⟩ | ⟨-, h2, -⟩
    · have := hk.j1 x hx; have := he.next_le; omega
    · exact h2
  · intro y hy
    rw [he.seen] at hy
    have := hk.j2 y hy; have := he.next_le; omega
  · intro y hy z hz hyz
    rcases he.old y hy with ⟨x, hx, h1, h2, -⟩ | ⟨h1, -, -⟩
    · rcases he.old z hz with ⟨x', hx', h1', h2', -⟩ | ⟨h1', -, -⟩
      · rw [← h2, ← h2']; exact hk.w x hx x' hx' (by omega)
      · exact (he.inj z hz y hy h1' hyz.symm).symm
    · exact he.inj y hy z hz h1 hyz
  · intro y hy x hx hyx
    rw [he.seen] at hy
    rcases he.old x hx with ⟨x0, hx0, h1, h2, -⟩ | ⟨h1, -, -⟩
    · rw [← h2]; exact hk.u y hy x0 hx0 (by omega)
    · have := hk.j2 y hy; omega
  · intro h8 hag' x hx ho hf hb
    rw [hrp] at h8; rw [hag] at hag'
    rw [he.seen]
    rcases he.old x hx with ⟨x0, hx0, h1, h2, h3⟩ | ⟨-, -, h3⟩
    · have := hk.k8 h8 hag' x0 hx0 (by rw [h2]; exact ho) (by rw [h2]; exact hf)
        (by unfold badGen at *; rw [h3]; exact hb)
      rw [← h1]; exact this
    · exfalso
      simp only [h8, CloneOk] at hi
      rcases h3 with h3 | ⟨-, h3⟩
      · unfold badGen at hb; rw [h3] at hb; simp at hb
      · cases hfam : x.kind.fam <;> simp_all [State.gateIn]
  · intro n hn h9 h11 hag' x hx ho hb
    rw [hrp] at hn; rw [hag] at hag'
    rw [he.man, he.lst]
    rcases he.old x hx with ⟨x0, hx0, h1, h2, h3⟩ | ⟨-, -, h3⟩
    · have := hk.k5 n hn h9 h11 hag' x0 hx0 (by rw [h2]; exact ho)
        (by unfold badGen at *; rw [h3]; exact hb)
      rw [← h1]; exact this
    · exfalso
      simp only [hn, CloneOk] at hi
      have hin : s.in4 = true ∧ s.in6 = true := hi.2.2.2.1 (by omega)
      have hsc' : s.sc = false := hi.2.2.2.2.2.2.2.2.2.2.2.2.2.1 (by omega) (by omega)
      rcases h3 with h3 | ⟨-, h3⟩
      · unfold badGen at hb; rw [h3] at hb; simp at hb
      · cases hfam : x.kind.fam <;> simp_all [State.gateIn]
  · intro hc x hx ho hb
    rw [hcut] at hc
    rcases he.old x hx with ⟨x0, hx0, h1, h2, h3⟩ | ⟨-, -, h3⟩
    · exact hk.k3 hc x0 hx0 (by rw [h2]; exact ho) (by unfold badGen at *; rw [h3]; exact hb)
    · rcases h3 with h3 | ⟨h3, h4⟩
      · unfold badGen at hb; rw [h3] at hb; simp at hb
      · cases hsh : s.shutTo x.kind.fam
        · unfold badGen at hb; rw [h3] at hb; simp [genIn, hsh] at hb
        · have hsc' := shut_cut_sc hs hpw hsh hc
          have hg : s.gateIn x.kind.fam = true := by
            simp [State.shutTo] at hsh; exact hsh.2
          rcases h4 with h4 | h4 <;> simp_all

/-! ## What each step of the environment does to the sockets -/

/-- The fields `Evo` relates. -/
def State.sk (s : State) := (s.socks, s.next, s.seen, s.lst, s.man)

@[simp] theorem ctSet_sk (s : State) (c : Cli) (p : Port) (v : Option CtSt) :
    (s.ctSet c p v).sk = s.sk := by
  cases c <;> cases p <;> rfl

@[simp] theorem ckSet_sk (s : State) (c : Cli) (p : Port) : (s.ckSet c p).sk = s.sk := by
  cases c <;> cases p <;> rfl

@[simp] theorem noteRst_sk (s : State) (b : Bool) : (s.noteRst b).sk = s.sk := rfl

@[simp] theorem noteLate_sk (s : State) (b : Bool) : (s.noteLate b).sk = s.sk := rfl

@[simp] theorem answered_sk (tr : Bool) (c : Cli) (p : Port) (seg : Seg) (r : Bool) (s : State) :
    (answered tr c p seg r s).sk = s.sk := by
  unfold answered
  split
  · rfl
  · split <;> simp

@[simp] theorem unanswered_sk (R : Rules) (C : Cfg) (c : Cli) (p : Port) (seg : Seg)
    (s : State) : (unanswered R C c p seg s).sk = s.sk := by
  unfold unanswered
  split <;> simp

@[simp] theorem rejected_sk (R : Rules) (C : Cfg) (c : Cli) (p : Port) (seg : Seg) (s : State) :
    (rejected R C c p seg s).sk = s.sk := by
  unfold rejected
  split <;> simp

theorem evo_same {s t : State} (h : t.sk = s.sk) (hj : ∀ x ∈ s.socks, x.id < s.next) :
    Evo s t := by
  simp only [State.sk, Prod.mk.injEq] at h
  obtain ⟨h1, h2, h3, h4, h5⟩ := h
  refine ⟨by omega, ?_, ?_, h3, h4, h5⟩
  · intro y hy
    rw [h1] at hy
    exact Or.inl ⟨y, hy, rfl, rfl, rfl⟩
  · intro y hy z hz hn _
    rw [h1] at hy
    have := hj y hy
    omega

/-- Sockets only go. -/
theorem evo_sub {s t : State} (h1 : ∀ y ∈ t.socks, y ∈ s.socks) (h2 : t.next = s.next)
    (h3 : t.seen = s.seen) (h4 : t.lst = s.lst) (h5 : t.man = s.man)
    (hj : ∀ x ∈ s.socks, x.id < s.next) : Evo s t := by
  refine ⟨by omega, ?_, ?_, h3, h4, h5⟩
  · intro y hy
    exact Or.inl ⟨y, h1 y hy, rfl, rfl, rfl⟩
  · intro y hy z hz hn _
    have := hj y (h1 y hy)
    omega

/-- A request socket becomes the established socket. -/
theorem evo_upgrade {s : State} (k : Kind) (hj : ∀ x ∈ s.socks, x.id < s.next) :
    Evo s (s.upgrade k) := by
  refine ⟨Nat.le_refl _, ?_, ?_, rfl, rfl, rfl⟩
  · intro y hy
    simp only [State.upgrade, List.mem_map] at hy
    obtain ⟨x, hx, rfl⟩ := hy
    refine Or.inl ⟨x, hx, ?_, ?_, ?_⟩ <;> split <;> rfl
  · intro y hy z hz hn _
    simp only [State.upgrade, List.mem_map] at hy
    obtain ⟨x, hx, rfl⟩ := hy
    have := hj x hx
    have : (if x.kind == k then { x with st := St.est } else x).id = x.id := by split <;> rfl
    omega

/-- One new socket, with the next cookie. -/
theorem evo_add {s : State} (k : Kind) (st : St) (g : Gen)
    (hg : g = .own ∨ (g = genIn s k.fam ∧ (s.gateIn k.fam = false ∨ s.sc = true)))
    (hj : ∀ x ∈ s.socks, x.id < s.next) : Evo s (s.add k st g) := by
  refine ⟨by simp [State.add], ?_, ?_, rfl, rfl, rfl⟩
  · intro y hy
    simp only [State.add, mem_ins] at hy
    rcases hy with rfl | hy
    · exact Or.inr ⟨Nat.le_refl _, by simp [State.add], hg⟩
    · exact Or.inl ⟨y, hy, rfl, rfl, rfl⟩
  · intro y hy z hz hn hyz
    simp only [State.add, mem_ins] at hy hz
    rcases hy with rfl | hy
    · rcases hz with rfl | hz
      · rfl
      · have := hj z hz; simp at hyz; omega
    · have := hj y hy; omega

theorem evo_trans {s t r : State} (h1 : Evo s t) (h2 : Evo t r)
    (hgen : ∀ y ∈ r.socks, t.next ≤ y.id → y.gen = .own) : Evo s r := by
  refine ⟨Nat.le_trans h1.next_le h2.next_le, ?_, ?_, by rw [h2.seen, h1.seen],
    by rw [h2.lst, h1.lst], by rw [h2.man, h1.man]⟩
  · intro y hy
    rcases h2.old y hy with ⟨x, hx, e1, e2, e3⟩ | ⟨e1, e2, -⟩
    · rcases h1.old x hx with ⟨x0, hx0, f1, f2, f3⟩ | ⟨f1, f2, f3⟩
      · exact Or.inl ⟨x0, hx0, by omega, by rw [f2, e2], by rw [f3, e3]⟩
      · refine Or.inr ⟨by omega, by have := h2.next_le; omega, ?_⟩
        rw [← e3, ← e2]; exact f3
    · exact Or.inr ⟨by have := h1.next_le; omega, e2, Or.inl (hgen y hy e1)⟩
  · intro y hy z hz hn hyz
    rcases h2.old y hy with ⟨x, hx, e1, e2, -⟩ | ⟨e1, -, -⟩
    · rcases h2.old z hz with ⟨x', hx', e1', e2', -⟩ | ⟨e1', -, -⟩
      · rw [← e2, ← e2']; exact h1.inj x hx x' hx' (by omega) (by omega)
      · exact (h2.inj z hz y hy e1' hyz.symm).symm
    · exact h2.inj y hy z hz e1 hyz

theorem evo_congr {s t t' : State} (h : t.sk = t'.sk) (he : Evo s t') : Evo s t := by
  simp only [State.sk, Prod.mk.injEq] at h
  obtain ⟨h1, h2, h3, h4, h5⟩ := h
  refine ⟨by rw [h2]; exact he.next_le, ?_, ?_, by rw [h3]; exact he.seen,
    by rw [h4]; exact he.lst, by rw [h5]; exact he.man⟩
  · intro y hy; rw [h1] at hy; rw [h2]; exact he.old y hy
  · intro y hy z hz; rw [h1] at hy hz; exact he.inj y hy z hz

theorem evo_drop {s : State} (k : Kind) (hj : ∀ x ∈ s.socks, x.id < s.next) :
    Evo s (s.drop k) :=
  evo_sub (fun y hy => by simp only [State.drop, List.mem_filter] at hy; exact hy.1)
    rfl rfl rfl rfl hj

theorem pkt_evo {C : Cfg} {c : Cli} {p : Port} {seg : Seg} {s t : State} :
    pkt Design.rules C c p seg s = some t → (∀ x ∈ s.socks, x.id < s.next) → Evo s t := by
  unfold pkt
  repeat' split
  all_goals (intro h hj)
  all_goals (try contradiction)
  all_goals (simp only [Option.some.injEq] at h; subst h)
  all_goals first
    | (refine evo_same ?_ hj; simp; done)
    | (refine evo_congr ?_ (evo_upgrade (.inb c p) hj); simp; done)
    | (refine evo_congr ?_ (evo_drop (.inb c p) hj); simp; done)
    | skip
  · -- a SYN that was not rejected found the ingress half out for its family
    rename_i hr _
    refine evo_congr ?_ (evo_add (.inb c p) .syn (genIn s c.fam) ?_ hj)
    · simp
    · simp [rejects, Design.rules] at hr
      exact Or.inr ⟨rfl, Or.inl hr⟩
  · -- a cookie's ACK found `tcp_syncookies` on
    have hsc : s.sc = true := by simp_all
    refine evo_congr ?_ (evo_add (.inb c p) .est (genIn s c.fam)
      (Or.inr ⟨rfl, Or.inr hsc⟩) hj)
    simp

theorem add_j1 {s : State} (k : Kind) (st : St) (g : Gen)
    (hj : ∀ x ∈ s.socks, x.id < s.next) :
    ∀ x ∈ (s.add k st g).socks, x.id < (s.add k st g).next := by
  intro x hx
  simp only [State.add, mem_ins] at hx ⊢
  rcases hx with rfl | hx
  · simp
  · have := hj x hx; omega

/-- A process opens two sockets at once: a connection inside the guest, or through the proxy. -/
theorem evo_add2 {s : State} (k1 k2 : Kind) (hj : ∀ x ∈ s.socks, x.id < s.next) :
    Evo s ((s.add k1 .est .own).add k2 .est .own) := by
  refine evo_trans (evo_add k1 .est .own (Or.inl rfl) hj)
    (evo_add k2 .est .own (Or.inl rfl) (add_j1 k1 .est .own hj)) ?_
  intro y hy hn
  simp only [State.add, mem_ins] at hy
  rcases hy with rfl | hy
  · rfl
  · have := add_j1 k1 .est .own hj y (by simpa [State.add, mem_ins] using hy)
    omega

/-- Closes `f s = some t → (∀ x ∈ s.socks, x.id < s.next) → Evo s t` for a step that leaves the
sockets alone, drops some, or adds a process's own. -/
macro "evo_env" : tactic => `(tactic| (
  repeat' split
  all_goals (intro h hj)
  all_goals (try contradiction)
  all_goals (simp only [Option.some.injEq] at h; subst h)
  all_goals first
    | (refine evo_same ?_ hj; simp [State.sk]; done)
    | (exact evo_add2 _ _ hj)
    | (exact evo_add _ _ _ (Or.inl rfl) hj)
    | (refine evo_sub ?_ rfl rfl rfl rfl hj
       intro y hy
       simp only [State.drop, List.mem_filter] at hy
       simp_all)))

theorem udpC_evo {R : Rules} {C : Cfg} {s t : State} :
    udpC R C s = some t → (∀ x ∈ s.socks, x.id < s.next) → Evo s t := by
  unfold udpC
  evo_env

theorem udpR_evo {C : Cfg} {s t : State} :
    udpR C s = some t → (∀ x ∈ s.socks, x.id < s.next) → Evo s t := by
  unfold udpR
  evo_env

theorem udpS_evo {R : Rules} {C : Cfg} {s t : State} :
    udpS R C s = some t → (∀ x ∈ s.socks, x.id < s.next) → Evo s t := by
  unfold udpS
  evo_env

theorem conn_evo {R : Rules} {C : Cfg} {w : Who} {s t : State} :
    conn R C w s = some t → (∀ x ∈ s.socks, x.id < s.next) → Evo s t := by
  unfold conn
  evo_env

theorem shut_evo {w : Who} {s t : State} :
    shut w s = some t → (∀ x ∈ s.socks, x.id < s.next) → Evo s t := by
  unfold shut
  evo_env

theorem pair_evo {C : Cfg} {b : Bool} {s t : State} :
    pair C b s = some t → (∀ x ∈ s.socks, x.id < s.next) → Evo s t := by
  unfold pair
  evo_env

theorem unpair_evo {b : Bool} {s t : State} :
    unpair b s = some t → (∀ x ∈ s.socks, x.id < s.next) → Evo s t := by
  unfold unpair
  evo_env

/-! ## The system's own steps -/

/-- A step that leaves the sockets alone, and either leaves the restore where it was or moves it
somewhere `k8` and `k5` say nothing about. -/
theorem sk_sys {s t : State} (hk : Sk s) (h1 : t.socks = s.socks) (h2 : t.next = s.next)
    (hseen : ∀ y ∈ t.seen, y ∈ s.seen ∨ y ∈ s.socks)
    (h8 : t.rp = .at 8 → t.ag = true → s.rp = .at 8 ∧ s.ag = true ∧ t.seen = s.seen)
    (h5 : ∀ n, t.rp = .at n → 9 ≤ n → n ≤ 11 → t.ag = true →
      ∃ m, s.rp = .at m ∧ 9 ≤ m ∧ m ≤ 11 ∧ s.ag = true ∧ t.man = s.man ∧ t.lst = s.lst)
    (h3 : t.cut = true → s.cut = true) : Sk t := by
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro x hx; rw [h1] at hx; rw [h2]; exact hk.j1 x hx
  · intro y hy
    rw [h2]
    rcases hseen y hy with h | h
    · exact hk.j2 y h
    · exact hk.j1 y h
  · intro x hx y hy; rw [h1] at hx hy; exact hk.w x hx y hy
  · intro y hy x hx
    rw [h1] at hx
    rcases hseen y hy with h | h
    · exact hk.u y h x hx
    · exact hk.w y h x hx
  · intro hr ha x hx
    obtain ⟨e1, e2, e3⟩ := h8 hr ha
    rw [h1] at hx; rw [e3]
    exact hk.k8 e1 e2 x hx
  · intro n hr h9 h11 ha x hx
    obtain ⟨m, e1, e2, e3, e4, e5, e6⟩ := h5 n hr h9 h11 ha
    rw [h1] at hx; rw [e5, e6]
    exact hk.k5 m e1 e2 e3 e4 x hx
  · intro hc x hx; rw [h1] at hx; exact hk.k3 (h3 hc) x hx

theorem outside_mirror {k : Kind} (h : k.outside = true) : k.mirror = none := by
  cases k <;> simp_all [Kind.outside, Kind.mirror]

theorem hasMirror_outside (l : List Sock) {x : Sock} (h : x.kind.outside = true) :
    hasMirror l x = false := by
  simp [hasMirror, outside_mirror h]

/-- The IPv4 dump. -/
theorem sk_dump4 {s t : State} (hk : Sk s) (h1 : t.socks = s.socks) (h2 : t.next = s.next)
    (hseen : t.seen = s.socks.filter (·.kind.fam == .v4)) (hrp : t.rp = .at 8)
    (hcut : t.cut = s.cut) : Sk t := by
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro x hx; rw [h1] at hx; rw [h2]; exact hk.j1 x hx
  · intro y hy
    rw [hseen, List.mem_filter] at hy; rw [h2]; exact hk.j1 y hy.1
  · intro x hx y hy; rw [h1] at hx hy; exact hk.w x hx y hy
  · intro y hy x hx
    rw [hseen, List.mem_filter] at hy; rw [h1] at hx
    exact hk.w y hy.1 x hx
  · intro _ _ x hx _ hf _
    rw [h1] at hx
    refine ⟨x, ?_, rfl⟩
    rw [hseen, List.mem_filter]
    exact ⟨hx, by simp [hf]⟩
  · intro n hn h9 _
    rw [hrp] at hn; cases hn; omega
  · intro hc x hx; rw [h1] at hx; rw [hcut] at hc; exact hk.k3 hc x hx

/-- The IPv6 dump, and the comparison with the manifest. -/
theorem sk_dump6 {s t : State} (hk : Sk s) (hs : s.rp = .at 8) (hag : s.ag = true)
    (h1 : t.socks = s.socks) (h2 : t.next = s.next)
    (hseen : t.seen = s.seen ++ s.socks.filter (·.kind.fam == .v6))
    (hlst : t.lst = listIds Design.rules (s.man.getD []) t.seen) (hman : t.man = s.man)
    (hrp : t.rp = .at 9) (hcut : t.cut = s.cut) : Sk t := by
  have hmem : ∀ y ∈ t.seen, y ∈ s.seen ∨ y ∈ s.socks := by
    intro y hy
    rw [hseen, List.mem_append, List.mem_filter] at hy
    rcases hy with h | h
    · exact Or.inl h
    · exact Or.inr h.1
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro x hx; rw [h1] at hx; rw [h2]; exact hk.j1 x hx
  · intro y hy
    rw [h2]
    rcases hmem y hy with h | h
    · exact hk.j2 y h
    · exact hk.j1 y h
  · intro x hx y hy; rw [h1] at hx hy; exact hk.w x hx y hy
  · intro y hy x hx
    rw [h1] at hx
    rcases hmem y hy with h | h
    · exact hk.u y h x hx
    · exact hk.w y h x hx
  · intro hr; rw [hrp] at hr; cases hr
  · intro n _ _ _ _ x hx ho hb
    rw [h1] at hx; rw [hman]
    -- the socket, or its seen copy, is in the dump
    have hy : ∃ y ∈ t.seen, y.id = x.id ∧ y.kind = x.kind := by
      cases hf : x.kind.fam
      · obtain ⟨y, hy, hid⟩ := hk.k8 hs hag x hx ho hf hb
        refine ⟨y, ?_, hid, hk.u y hy x hx hid⟩
        rw [hseen, List.mem_append]; exact Or.inl hy
      · refine ⟨x, ?_, rfl, rfl⟩
        rw [hseen, List.mem_append, List.mem_filter]; exact Or.inr ⟨hx, by simp [hf]⟩
    obtain ⟨y, hy, hid, hkind⟩ := hy
    by_cases hm : x.id ∈ s.man.getD []
    · exact Or.inl hm
    · refine Or.inr ?_
      rw [hlst]
      simp only [listIds, Design.rules, List.mem_map, List.mem_filter]
      refine ⟨y, ⟨hy, ?_⟩, hid⟩
      have : hasMirror t.seen y = false := hasMirror_outside _ (by rw [hkind]; exact ho)
      simp [this, hid, hm]
  · intro hc x hx; rw [h1] at hx; rw [hcut] at hc; exact hk.k3 hc x hx

/-- The second look. -/
theorem sk_look {s t : State} (hk : Sk s) (hs : s.rp = .at 9) (hag : s.ag = true)
    (h1 : t.socks = s.socks) (h2 : t.next = s.next) (hseen : t.seen = s.seen)
    (hlst : t.lst = lookIds s.lst s.seen s.socks) (hman : t.man = s.man)
    (hrp : t.rp = .at 10) (hcut : t.cut = s.cut) : Sk t := by
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro x hx; rw [h1] at hx; rw [h2]; exact hk.j1 x hx
  · intro y hy; rw [hseen] at hy; rw [h2]; exact hk.j2 y hy
  · intro x hx y hy; rw [h1] at hx hy; exact hk.w x hx y hy
  · intro y hy x hx; rw [hseen] at hy; rw [h1] at hx; exact hk.u y hy x hx
  · intro hr; rw [hrp] at hr; cases hr
  · intro n _ _ _ _ x hx ho hb
    rw [h1] at hx; rw [hman]
    rcases hk.k5 9 hs (by omega) (by omega) hag x hx ho hb with h | h
    · exact Or.inl h
    · refine Or.inr ?_
      rw [hlst]
      simp only [lookIds, List.mem_filter]
      refine ⟨h, ?_⟩
      simp only [Bool.not_eq_true', List.any_eq_false]
      intro y hy
      by_cases hid : y.id = x.id
      · have : hasMirror s.socks y = false :=
          hasMirror_outside _ (by rw [hk.u y hy x hx hid]; exact ho)
        simp [this]
      · simp [hid]
  · intro hc x hx; rw [h1] at hx; rw [hcut] at hc; exact hk.k3 hc x hx

/-- The retirement: what the manifest and the list name is gone, and that is everything that
had to go. -/
theorem sk_retire {s t : State} (hk : Sk s) (hs : s.rp = .at 11) (hag : s.ag = true)
    (h1 : t.socks = s.socks.filter (fun x => !(s.man.getD [] ++ s.lst).contains x.id))
    (h2 : t.next = s.next) (hseen : t.seen = s.seen) (hrp : t.rp = .at 12) : Sk t := by
  have hsub : ∀ x ∈ t.socks, x ∈ s.socks := by
    intro x hx; rw [h1, List.mem_filter] at hx; exact hx.1
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · intro x hx; rw [h2]; exact hk.j1 x (hsub x hx)
  · intro y hy; rw [hseen] at hy; rw [h2]; exact hk.j2 y hy
  · intro x hx y hy; exact hk.w x (hsub x hx) y (hsub y hy)
  · intro y hy x hx; rw [hseen] at hy; exact hk.u y hy x (hsub x hx)
  · intro hr; rw [hrp] at hr; cases hr
  · intro n hn h9 _
    rw [hrp] at hn; cases hn; omega
  · intro _ x hx ho hb
    rw [h1, List.mem_filter] at hx
    obtain ⟨hx, hnot⟩ := hx
    have := hk.k5 11 hs (by omega) (by omega) hag x hx ho hb
    simp only [Bool.not_eq_true', List.contains_eq_mem, List.mem_append,
      decide_eq_false_iff_not] at hnot
    exact hnot this

theorem step_pw {R : Rules} {C : Cfg} {l : Label} {s t : State}
    (h : step R C l s = some t) : s.pw = true := by
  cases hp : s.pw
  · exfalso
    cases l <;>
      simp [step, pkt, udpC, udpR, udpS, conn, shut, pair, unpair, snap, snapFail, snapKill,
        restore, agent, agentFail, kill, restart, off, timeout, watchdog, State.running,
        agentRuns, hp] at h
  · rfl

set_option hygiene false in
macro "open_sk" : tactic => `(tactic| (
  repeat' split
  all_goals (intro h hi hk)
  all_goals (try contradiction)
  all_goals (try simp only [Option.ite_none_right_eq_some, Option.ite_none_left_eq_some,
    Option.some.injEq, reduceCtorEq] at h)
  all_goals (try contradiction)
  all_goals (repeat (obtain ⟨_, h⟩ := h))
  all_goals (try subst h)))

set_option hygiene false in
macro "close_sk" : tactic => `(tactic| (
  refine sk_sys hk rfl rfl ?_ ?_ ?_ ?_
  · intro y hy
    first
      | exact Or.inl hy
      | (simp at hy; done)
      | (simp only [List.mem_filter] at hy; exact Or.inr hy.1)
      | (simp only [List.mem_append, List.mem_filter] at hy
         rcases hy with hy | hy
         · exact Or.inl hy
         · exact Or.inr hy.1)
  · intro hr ha
    have hi' := hi (by simp_all [State.running])
    first
      | (simp_all [CloneOk, FailedOk, SrcOk, State.unpublished, openGate, State.closeGate,
          State.dead]; done)
      | (simp_all [CloneOk, FailedOk, SrcOk, State.unpublished, openGate, State.closeGate,
          State.dead]; omega)
  · intro n hr h9 h11 ha
    have hi' := hi (by simp_all [State.running])
    first
      | (simp_all [CloneOk, FailedOk, SrcOk, State.unpublished, openGate, State.closeGate,
          State.dead]; done)
      | (simp_all [CloneOk, FailedOk, SrcOk, State.unpublished, openGate, State.closeGate,
          State.dead]; omega)
  · intro hc
    simp_all [openGate, State.closeGate, State.dead]))

theorem snap_sk {C : Cfg} {g : Bool} {s t : State} :
    snap Design.rules C g s = some t → Scal C s → Sk s → Sk t := by
  unfold snap
  open_sk
  all_goals close_sk

theorem snapFail_sk {C : Cfg} {s t : State} :
    snapFail Design.rules C s = some t → Scal C s → Sk s → Sk t := by
  unfold snapFail
  open_sk
  all_goals close_sk

theorem snapKill_sk {C : Cfg} {s t : State} :
    snapKill C s = some t → Scal C s → Sk s → Sk t := by
  unfold snapKill
  open_sk
  all_goals close_sk

theorem kill_sk {C : Cfg} {s t : State} : kill C s = some t → Scal C s → Sk s → Sk t := by
  unfold kill
  open_sk
  all_goals close_sk

theorem restart_sk {C : Cfg} {s t : State} :
    restart Design.rules C s = some t → Scal C s → Sk s → Sk t := by
  unfold restart
  open_sk
  all_goals close_sk

theorem watchdog_sk {C : Cfg} {s t : State} :
    watchdog Design.rules C s = some t → Scal C s → Sk s → Sk t := by
  unfold watchdog
  open_sk
  all_goals close_sk

theorem sk_dead (s : State) : Sk s.dead := by
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_⟩ <;> simp [State.dead]

theorem off_sk {s t : State} : off s = some t → Sk t := by
  unfold off
  split
  · intro h; simp only [Option.some.injEq] at h; subst h; exact sk_dead s
  · intro h; contradiction

theorem timeout_sk {s t : State} : timeout s = some t → Sk t := by
  unfold timeout
  split
  · intro h; simp only [Option.some.injEq] at h; subst h; exact sk_dead s
  · intro h; contradiction

/-- A clone is restored: the same sockets, each now the source's, and nothing seen or listed. -/
theorem restore_sk {C : Cfg} {s t : State} : restore C s = some t → Sk s → Sk t := by
  unfold restore
  split
  · intro h hk
    simp only [Option.some.injEq] at h
    subst h
    refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
    · intro x hx
      simp only [List.mem_map] at hx
      obtain ⟨x0, hx0, rfl⟩ := hx
      exact hk.j1 x0 hx0
    · intro y hy; simp at hy
    · intro x hx y hy
      simp only [List.mem_map] at hx hy
      obtain ⟨x0, hx0, rfl⟩ := hx
      obtain ⟨y0, hy0, rfl⟩ := hy
      exact hk.w x0 hx0 y0 hy0
    · intro y hy; simp at hy
    · intro hr; split at hr <;> simp at hr
    · intro n hr h9; split at hr <;> simp at hr; omega
    · intro hc; simp at hc
  · intro h; contradiction

theorem agent_spec {R : Rules} {C : Cfg} {s t : State} (h : agent R C s = some t) :
    ∃ n r, s.rp = .at n ∧ R.restore[n]? = some r ∧ s.ag = true
      ∧ t = (if (r == .load && s.man.isNone) = true then { s with rp := .failed, saved := none }
             else advance R (doStep R C r s)) := by
  unfold agent at h
  split at h
  · contradiction
  · rename_i hrun
    have hag : s.ag = true := by simp [agentRuns] at hrun; exact hrun.1.2
    cases hr : s.rp with
    | «at» n =>
      simp only [nextStep, hr] at h
      split at h
      · contradiction
      · rename_i r hnr
        refine ⟨n, r, rfl, hnr, hag, ?_⟩
        split at h <;> simp_all
    | _ => simp [nextStep, hr] at h

theorem agent_sk {C : Cfg} {s t : State} (h : agent Design.rules C s = some t)
    (hi : Scal C s) (hk : Sk s) : Sk t := by
  have hpw := agent_pw h
  obtain ⟨n, r, hr, hnr, hag, rfl⟩ := agent_spec h
  have hi' := hi hpw
  simp only [hr] at hi'
  have hn : n < 18 := hi'.1
  rcases lt18 hn with h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0 | h0
    | h0 | h0 | h0 | h0 <;> subst h0 <;> simp [Design.rules] at hnr <;> subst hnr
  all_goals first
    | -- a step that leaves the sockets, the dump and the lists alone
      (refine sk_sys hk ?_ ?_ ?_ ?_ ?_ ?_
       · simp [advance, doStep, hr, Design.rules, openGate, State.closeGate]
       · simp [advance, doStep, hr, Design.rules, openGate, State.closeGate]
       · intro y hy
         simp [advance, doStep, hr, Design.rules, openGate, State.closeGate] at hy <;>
           exact Or.inl hy
       · simp [advance, doStep, hr, Design.rules, openGate, State.closeGate]
       · intro m hm h9 h11
         simp [advance, doStep, hr, Design.rules, openGate, State.closeGate] at hm <;> omega
       · simp [advance, doStep, hr, Design.rules, openGate, State.closeGate]
       done)
    | skip
  · -- load: with no manifest the restore fails; with one, nothing changes
    cases hm : s.man.isNone <;>
      (refine sk_sys hk ?_ ?_ ?_ ?_ ?_ ?_ <;> simp [advance, doStep, hr, Design.rules, hm]) <;>
      first
        | (intro y hy; exact Or.inl hy)
        | (intro m hm' h9 h11; omega)
  · refine sk_dump4 hk ?_ ?_ ?_ ?_ ?_ <;> simp [advance, doStep, hr, Design.rules]
  · refine sk_dump6 hk hr hag ?_ ?_ ?_ ?_ ?_ ?_ ?_ <;> simp [advance, doStep, hr, Design.rules]
  · refine sk_look hk hr hag ?_ ?_ ?_ ?_ ?_ ?_ ?_ <;> simp [advance, doStep, hr, Design.rules]
  · -- egress: the list stands, the dump is dropped
    (refine sk_sys hk ?_ ?_ ?_ ?_ ?_ ?_ <;> simp [advance, doStep, hr, Design.rules]) <;>
      first
        | (intro y hy; exact Or.inl hy)
        | (intro m hm h9 h11; exact ⟨10, rfl, by omega, by omega, hag⟩)
        | exact ⟨10, rfl, by omega, by omega, hag⟩
  · refine sk_retire hk hr hag ?_ ?_ ?_ ?_ <;> simp [advance, doStep, hr, Design.rules]

theorem agentFail_sk {C : Cfg} {s t : State} :
    agentFail Design.rules C s = some t → Sk s → Sk t := by
  unfold agentFail
  repeat' split
  all_goals (intro h hk)
  all_goals (try contradiction)
  all_goals first
    | (exfalso; simp_all; done)
    | (simp only [Option.some.injEq] at h
       subst h
       refine sk_sys hk rfl rfl ?_ ?_ ?_ ?_ <;> simp)

/-- Every step of the design keeps `Sk`. -/
theorem step_sk {C : Cfg} {l : Label} {s t : State}
    (h : step Design.rules C l s = some t) (hi : Scal C s) (hk : Sk s) : Sk t := by
  have hpw := step_pw h
  cases l with
  | pkt c p seg => exact sk_evo hi hpw (pkt_evo h hk.j1) (pkt_scal h) hk
  | udpC => exact sk_evo hi hpw (udpC_evo h hk.j1) (udpC_scal h) hk
  | udpR => exact sk_evo hi hpw (udpR_evo h hk.j1) (udpR_scal h) hk
  | udpS => exact sk_evo hi hpw (udpS_evo h hk.j1) (udpS_scal h) hk
  | detach =>
    simp only [step] at h
    split at h
    · simp only [Option.some.injEq] at h
      subst h
      exact sk_evo hi hpw (evo_same rfl hk.j1) rfl hk
    · contradiction
  | conn w => exact sk_evo hi hpw (conn_evo h hk.j1) (conn_scal h) hk
  | shut w => exact sk_evo hi hpw (shut_evo h hk.j1) (shut_scal h) hk
  | pair b => exact sk_evo hi hpw (pair_evo h hk.j1) (pair_scal h) hk
  | unpair b => exact sk_evo hi hpw (unpair_evo h hk.j1) (unpair_scal h) hk
  | snap => exact snap_sk h hi hk
  | snapFail => exact snapFail_sk h hi hk
  | snapKill => exact snapKill_sk h hi hk
  | hostDie =>
    simp only [step] at h
    split at h
    · simp only [Option.some.injEq] at h
      subst h
      exact sk_evo hi hpw (evo_same rfl hk.j1) rfl hk
    · contradiction
  | guard => exact snap_sk h hi hk
  | restore => exact restore_sk h hk
  | agent => exact agent_sk h hi hk
  | agentFail => exact agentFail_sk h hk
  | kill => exact kill_sk h hi hk
  | restart => exact restart_sk h hi hk
  | off => exact off_sk h
  | timeout => exact timeout_sk h
  | watchdog => exact watchdog_sk h hi hk

theorem sk_init (C : Cfg) : Sk (init C) := by
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_⟩ <;> simp [init]

theorem reachable_sk {C : Cfg} {s : State} (hr : Reachable Design.rules C s) : Sk s := by
  induction hr with
  | init => exact sk_init C
  | step l hs h ih => exact step_sk h (reachable_scal hs) ih

end Gate
