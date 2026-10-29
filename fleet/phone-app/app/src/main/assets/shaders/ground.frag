// The lawn: two greens in soft patches, mown stripes near the camera, two sandy paths.
in vec3 v_world;
in vec4 v_ls;
out vec4 o;

float lattice(ivec2 i) { return hash2(uint(i.x), uint(i.y) * 7919u); }
float vnoise(vec2 p) {
    vec2 f = fract(p);
    ivec2 i = ivec2(floor(p));
    vec2 u = f * f * (3.0 - 2.0 * f);
    float a = lattice(i), b = lattice(i + ivec2(1, 0)), c = lattice(i + ivec2(0, 1)), d = lattice(i + ivec2(1, 1));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

void main() {
    vec2 p = v_world.xz;
    float dist = length(v_world - u_eye);
    float n = vnoise(p * 0.06) * 0.55 + vnoise(p * 0.21 + 17.0) * 0.3 + vnoise(p * 0.8 + 41.0) * 0.15;
    vec3 grass = mix(palette(13.0), palette(12.0), smoothstep(0.25, 0.75, n));
    float stripe = smoothstep(-0.25, 0.25, sin(p.x * PI / 4.0));
    grass *= 1.0 + 0.1 * (stripe - 0.5) * exp(-dist / 60.0);
    float dz = min(abs(p.y - 5.0), abs(p.y + 19.0));
    float path = 1.0 - smoothstep(1.3, 1.7, dz + (vnoise(p * 1.3) - 0.5) * 0.3);
    vec3 sand = vec3(0.92, 0.66, 0.38) * (0.92 + 0.16 * vnoise(p * 2.7));
    vec3 base = mix(grass, sand, path);
    vec3 c = shade(base, vec3(0.0, 1.0, 0.0), v_world, v_ls, 0.0);
    o = vec4(finish(c, v_world), 1.0);
}
