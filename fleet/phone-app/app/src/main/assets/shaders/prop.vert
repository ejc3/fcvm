// Everything else in the park that is a mesh per instance: buddies, trees, flowers, hills,
// clouds and the viewers' light orbs. i_a = (x, y, z, yaw), i_b = (scale xyz, motion),
// i_c = (colour, second colour, kind, seed); kinds are Crowd.PROP_*.
layout(location = 0) in vec3 a_pos;
layout(location = 1) in vec3 a_nrm;
layout(location = 2) in vec2 a_part;
layout(location = 3) in vec4 i_a;
layout(location = 4) in vec4 i_b;
layout(location = 5) in vec4 i_c;
#ifndef SHADOW
out vec3 v_world;
out vec3 v_nrm;
out vec3 v_col;
out float v_emit;
out float v_spec;
out vec4 v_ls;
#endif

void main() {
    int kind = int(i_c.z + 0.5);
    float seed = i_c.w;
    vec3 pos = i_a.xyz;
    vec3 s = i_b.xyz;
    float yaw = i_a.w;
    vec3 p = a_pos;
    float mat = a_part.y;
    vec3 col = mat < 0.5 ? palette(i_c.x) : (mat < 1.5 ? palette(i_c.y) : (mat < 2.5 ? palette(8.0) : palette(9.0)));
    float emit = 0.0;
    float spec = 0.08;
    if (kind == 1) {
        // a cloud puff drifting with its cloud
        float x = pos.x + i_b.w * (CLOUD_LOOP / PERIOD) * u_tw;
        pos.x = x - CLOUD_LOOP * floor(x / CLOUD_LOOP + 0.5);
        emit = mix(0.45, 0.04, u_night);
        spec = 0.0;
    } else if (kind == 2) {
        // a buddy: bounces along its lane, and bounces to the beat when the crowd dances
        float x = pos.x + i_b.w * (LOOP_X / PERIOD) * u_walkT;
        float hops = floor(1500.0 + 600.0 * seed);
        float u = fract(hops * (u_walkT / PERIOD) + seed);
        float air = 4.0 * u * (1.0 - u);
        float still = max(u_dance, u_wave);
        float bounce = 0.35 * air * (1.0 - still) + u_dance * 0.22 * abs(sin(u_beat * PI + seed * 2.0));
        p.y *= mix(mix(0.82, 1.1, air), 1.0, still);
        pos = vec3(wrapX(x), bounce, pos.z);
        yaw = (i_b.w < 0.0 ? -0.9 : 0.9) * (1.0 - still);
        spec = 0.5;
        if (mat > 1.5 && mat < 2.5) emit = u_night * 1.6;
    } else if (kind == 3) {
        // a tree: the crown sways
        if (a_part.x < 0.5) p.x += sin(u_tw * 1.1 + seed * TAU) * 0.035 * p.y;
    } else if (kind == 4) {
        // a viewer's light: a glowing orb in its own colour
        vec3 lc = u_lightCol[int(i_c.x + 0.5)];
        pos = u_lightPos[int(i_c.x + 0.5)];
        float m = max(max(lc.r, lc.g), max(lc.b, 1e-4));
        col = lc / m;
        emit = 1.3 * min(m, 1.0);
        s *= min(m, 1.0);
        spec = 0.0;
    }
    vec3 world = pos + rotY(p * s, yaw);
#ifdef SHADOW
    gl_Position = u_lightViewProj * vec4(world, 1.0);
#else
    vec3 wn = normalize(rotY(a_nrm / s, yaw));
    gl_Position = u_viewProj * vec4(world, 1.0);
    v_world = world;
    v_nrm = wn;
    v_col = col;
    v_emit = emit;
    v_spec = spec;
    v_ls = u_lightViewProj * vec4(world + wn * 0.04, 1.0);
#endif
}
