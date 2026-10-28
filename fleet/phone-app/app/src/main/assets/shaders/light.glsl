// Lighting: the sun with soft shadows, sky and ground ambient, a rim, the viewers' lights, fog.
uniform highp sampler2DShadow u_shadow;
uniform float u_shadowTexel;

float shadowAt(vec4 ls) {
    vec3 p = ls.xyz / ls.w * 0.5 + 0.5;
    if (p.x <= 0.0 || p.x >= 1.0 || p.y <= 0.0 || p.y >= 1.0 || p.z >= 1.0) return 1.0;
    float z = p.z - 0.0004;
    float o = 0.75 * u_shadowTexel;
    float s = texture(u_shadow, vec3(p.xy + vec2(-o, -o), z));
    s += texture(u_shadow, vec3(p.xy + vec2(o, -o), z));
    s += texture(u_shadow, vec3(p.xy + vec2(-o, o), z));
    s += texture(u_shadow, vec3(p.xy + vec2(o, o), z));
    return s * 0.25;
}

vec3 shade(vec3 base, vec3 n, vec3 world, vec4 ls, float specK) {
    vec3 v = normalize(u_eye - world);
    float ndl = dot(n, u_sunDir);
    float sh = ndl > 0.0 ? shadowAt(ls) : 0.0;
    float hemi = 0.5 + 0.5 * n.y;
    vec3 amb = mix(u_groundCol, u_skyCol, hemi);
    vec3 c = base * (amb + u_sunCol * max(ndl, 0.0) * sh);
    vec3 h = normalize(u_sunDir + v);
    c += u_sunCol * specK * pow(max(dot(n, h), 0.0), 40.0) * sh;
    float rim = pow(1.0 - max(dot(n, v), 0.0), 3.0);
    c += (u_skyCol + base * 0.3) * rim * 0.4;
    for (int i = 0; i < 16; i++) {
        if (i >= u_lightCount) break;
        vec3 d = u_lightPos[i] - world;
        float r2 = dot(d, d);
        vec3 l = d * inversesqrt(max(r2, 1e-4));
        float att = 1.0 / (1.0 + r2 * 0.35);
        c += (base + vec3(0.15)) * u_lightCol[i] * (max(dot(n, l), 0.0) * 0.8 + 0.2) * att * 5.0;
    }
    return c;
}

vec3 finish(vec3 c, vec3 world) {
    float d = length(world - u_eye);
    float f = 1.0 - exp(-max(d - 60.0, 0.0) * 0.0028);
    return tone(mix(c, u_fogCol, f));
}
