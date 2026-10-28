// Confetti when the crowd dances, bubbles by day and fireflies by night, sparkles where the
// leapers land, and a glow around each viewer's light. Billboards facing the whole image's
// camera. i_p = (kind, index, seed, extra); i_a, i_b = the leaper a sparkle follows.
layout(location = 0) in vec2 a_corner;
layout(location = 1) in vec4 i_p;
layout(location = 2) in vec4 i_a;
layout(location = 3) in vec4 i_b;
out vec2 v_uv;
out vec4 v_col;
out float v_shape;

void main() {
    int kind = int(i_p.x + 0.5);
    float idx = i_p.y;
    float seed = i_p.z;
    float ex = i_p.w;
    vec3 c = vec3(0.0);
    float size = 0.0;
    vec4 col = vec4(0.0);
    float shape = 0.0;
    vec2 corner = a_corner;
    bool show = true;
    if (kind == 0) {
        float age = u_danceAge - seed * 2.5;
        if (age < 0.0 || age > 8.0) show = false;
        c = vec3(mix(u_spread.x, u_spread.y, ex) + 0.7 * sin(age * 2.3 + seed * 40.0),
                 15.0 + 7.0 * hash2(uint(idx), 3u) - 2.1 * age,
                 -26.0 + 34.0 * hash2(uint(idx), 5u));
        if (c.y < 0.02) show = false;
        size = 0.2;
        float spin = age * (4.0 + 4.0 * seed) + seed * TAU;
        corner = vec2(corner.x * cos(spin * 1.3), corner.y * 0.6);
        float cs = cos(spin), sn = sin(spin);
        corner = vec2(cs * corner.x - sn * corner.y, sn * corner.x + cs * corner.y);
        col = vec4(palette(1.0 + mod(idx, 7.0)) * 1.6, 1.0);
        shape = 0.0;
    } else if (kind == 1) {
        float x = wrapX((ex - 0.5) * LOOP_X + 0.6 * sin(u_tw * 0.31 + seed * TAU));
        float z = -38.0 + 54.0 * hash2(uint(idx), 9u);
        if (u_night < 0.5) {
            float y = mod(seed * 9.0 + u_tw * (0.18 + 0.2 * ex), 9.0);
            c = vec3(x + 0.3 * sin(u_tw * 1.7 + seed * 50.0), 0.3 + y, z);
            size = 0.16 + 0.12 * seed;
            col = vec4(0.85, 0.95, 1.0, 0.55 * (1.0 - smoothstep(7.5, 9.0, y)));
            shape = 1.0;
        } else {
            c = vec3(x, 0.4 + 2.2 * hash2(uint(idx), 11u) + 0.35 * sin(u_tw * 0.9 + seed * 30.0), z);
            size = 0.22;
            float blink = 0.5 + 0.5 * sin(u_tw * (1.5 + seed) + seed * 60.0);
            col = vec4(vec3(0.75, 1.0, 0.35) * 2.5 * blink * blink, 0.0);
            shape = 2.0;
        }
    } else if (kind == 2) {
        float cyc = i_a.w;
        float q = cyc * (u_walkT / PERIOD) + i_b.w;
        float k = floor(q);
        float age = (q - k) * PERIOD / cyc;
        float dir = i_a.z < 0.0 ? -1.0 : 1.0;
        float len = abs(i_a.z) * LOOP_X / cyc;
        vec3 land = vec3(wrapX(i_a.x + dir * k * len), 0.05, i_a.y);
        if (age > 1.1 || u_dance > 0.5) show = false;
        float ang = idx / 10.0 * TAU + seed * 0.6;
        vec3 vel = vec3(cos(ang) * 2.4, 2.6 + 1.6 * ex, sin(ang) * 1.4);
        // Each sparkle starts on a ring along its own direction. Started from one point, a
        // leaper's sparkles drew coincident quads just after it landed, and the T4 drew such a
        // frame two ways from one run to the next (1 pixel, 1 level, in 5 of 24 captures).
        c = land + vec3(cos(ang) * 0.3, 0.15 * ex, sin(ang) * 0.2) + vel * age + vec3(0.0, -4.9 * age * age, 0.0);
        if (c.y < 0.0) show = false;
        size = 0.2 * (1.0 - age / 1.1);
        col = vec4(mix(vec3(1.0, 0.85, 0.3), vec3(1.0), ex) * 3.0, 0.0);
        shape = 3.0;
    } else {
        int li = int(idx + 0.5);
        if (li >= u_lightCount) show = false;
        else {
            c = u_lightPos[li];
            size = 2.2;
            col = vec4(u_lightCol[li] * 0.5, 0.0);
            shape = 2.0;
        }
    }
    if (!show) {
        gl_Position = vec4(0.0, 0.0, 2.0, 1.0);
        v_uv = vec2(0.0); v_col = vec4(0.0); v_shape = 0.0;
        return;
    }
    vec3 world = c + (u_camRight * corner.x + u_camUp * corner.y) * size;
    gl_Position = u_viewProj * vec4(world, 1.0);
    v_uv = a_corner;
    v_col = col;
    v_shape = shape;
}
