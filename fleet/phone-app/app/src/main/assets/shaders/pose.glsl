// A robot's pose from its table entry (Crowd.robot) and the frame's clocks.
// a = (x0, z, laps, steps or cycles), b = (colour, scale, kind, seed).
struct Pose {
    vec3 root;
    float yaw;
    float legL, legR;
    float armSwingL, armSwingR;
    float armRaiseL, armRaiseR;
    float headTilt, headNod;
    float lift;
    float squash;
};

Pose robotPose(vec4 a, vec4 b) {
    Pose P;
    float kind = b.z;
    float seed = b.w;
    float dir = a.z < 0.0 ? -1.0 : 1.0;
    float stepPh = 0.0;
    float y = 0.0;
    float air = 0.0;
    float flight = 0.0;
    float x;
    float yaw = dir * 1.05;
    if (kind > 3.5) {
        // a sign carrier: placed by the CPU (FrameState.signs)
        x = a.x;
        stepPh = a.z;
        yaw = 0.55;
    } else if (kind > 1.5) {
        // a leaper: a short run-up, then a long arc to the next spot
        float c = a.w;
        float q = c * (u_walkT / PERIOD) + seed;
        float k = floor(q);
        float f = q - k;
        float len = abs(a.z) * LOOP_X / c;
        float g;
        if (f < 0.45) {
            g = 0.12 * f / 0.45;
            stepPh = f / 0.45 * 4.0 * TAU;
        } else {
            float s = (f - 0.45) / 0.55;
            g = 0.12 + 0.88 * s;
            flight = 1.0;
            y = 3.6 * 4.0 * s * (1.0 - s);
            air = s;
        }
        x = a.x + dir * (k + g) * len;
    } else {
        x = a.x + a.z * (LOOP_X / PERIOD) * u_walkT;
        if (kind > 0.5) {
            float u = fract(a.w * (u_walkT / PERIOD) + seed);
            air = 4.0 * u * (1.0 - u);
            y = 0.42 * air;
        } else {
            stepPh = fract(a.w * (u_walkT / PERIOD) + seed) * TAU;
            float n = floor(110.0 + 90.0 * seed);
            float u = fract(n * (u_walkT / PERIOD) + seed * 7.0);
            if (u < 0.07) { float s = u / 0.07; y = 0.55 * 4.0 * s * (1.0 - s); }
        }
    }
    float still = max(u_dance, u_wave);
    float walkW = 1.0 - still;
    P.root = vec3(wrapX(x), 0.0, a.y);
    float sw = sin(stepPh);
    P.legL = 0.55 * sw; P.legR = -0.55 * sw;
    P.armSwingL = -0.6 * sw; P.armSwingR = 0.6 * sw;
    P.armRaiseL = 0.12; P.armRaiseR = 0.12;
    P.headTilt = 0.07 * sin(stepPh * 0.5 + seed * 6.0);
    P.headNod = 0.0;
    P.lift = y + 0.03 * abs(cos(stepPh));
    P.squash = 1.0;
    if (kind > 3.5) {
        P.armSwingL = -2.25; P.armSwingR = -2.25;
        P.armRaiseL = 0.2; P.armRaiseR = 0.2;
    } else if (flight > 0.5) {
        P.legL = -0.9; P.legR = 0.45;
        P.armSwingL = 0.0; P.armSwingR = 0.0;
        P.armRaiseL = 2.4; P.armRaiseR = 2.4;
        P.headNod = -0.25 + 0.4 * air;
    } else if (kind > 0.5 && kind < 1.5) {
        P.legL = 0.0; P.legR = 0.0;
        P.armSwingL = 0.0; P.armSwingR = 0.0;
        P.armRaiseL = 0.3 + 1.6 * air; P.armRaiseR = 0.3 + 1.6 * air;
        P.squash = mix(0.86, 1.08, air);
    }
    P.lift *= walkW;
    P.squash = mix(P.squash, 1.0, still);
    // the dance: the whole crowd on one beat (120 bpm), arms up, a twist and a bounce
    float d = u_dance;
    float bt = u_beat * PI;
    float s1 = sin(bt);
    float twist = 0.0;
    if (d > 0.0) {
        float style = floor(seed * 3.0);
        twist = 0.45 * sin(bt * 0.5);
        P.lift += d * 0.16 * abs(s1);
        P.armRaiseL = mix(P.armRaiseL, style < 1.0 ? 2.7 + 0.35 * s1 : 1.6 + 0.9 * s1, d);
        P.armRaiseR = mix(P.armRaiseR, style < 1.0 ? 2.7 - 0.35 * s1 : 1.6 - 0.9 * s1, d);
        P.armSwingL = mix(P.armSwingL, style > 1.5 ? -1.2 * s1 : 0.0, d);
        P.armSwingR = mix(P.armSwingR, style > 1.5 ? 1.2 * s1 : 0.0, d);
        P.legL = mix(P.legL, 0.4 * max(s1, 0.0), d);
        P.legR = mix(P.legR, 0.4 * max(-s1, 0.0), d);
        P.headTilt = mix(P.headTilt, 0.3 * s1, d);
    }
    float w = u_wave;
    if (w > 0.0) {
        P.armRaiseR = mix(P.armRaiseR, 2.75 + 0.35 * sin(u_tw * 9.0 + seed * TAU), w);
        P.armSwingR = mix(P.armSwingR, -0.25, w);
        P.armRaiseL = mix(P.armRaiseL, 0.1, w);
        P.armSwingL = mix(P.armSwingL, 0.0, w);
        P.legL = mix(P.legL, 0.0, w); P.legR = mix(P.legR, 0.0, w);
        P.headTilt = mix(P.headTilt, -0.2, w);
    }
    P.yaw = mix(yaw, twist, still);
    return P;
}
