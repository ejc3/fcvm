out vec4 o;

void main() {
    // the framebuffer is the whole image (SceneRenderer), so this is the whole-image pixel
    vec2 ndc = gl_FragCoord.xy / u_vpSize * 2.0 - 1.0;
    vec4 a = u_invViewProj * vec4(ndc, -1.0, 1.0);
    vec4 b = u_invViewProj * vec4(ndc, 1.0, 1.0);
    vec3 dir = normalize(b.xyz / b.w - a.xyz / a.w);
    float e = dir.y;
    vec3 c = mix(u_horizonCol, u_zenithCol, pow(clamp(e, 0.0, 1.0), 0.4));
    c = mix(u_fogCol, c, smoothstep(-0.02, 0.06, e));
    float sd = dot(dir, u_sunDir);
    if (u_night < 0.5) {
        c += u_sunCol * (smoothstep(0.99935, 0.9996, sd) * 3.0 + pow(max(sd, 0.0), 300.0) * 0.5 + pow(max(sd, 0.0), 10.0) * 0.06);
    } else {
        c += vec3(0.9, 0.92, 1.0) * smoothstep(0.9991, 0.9994, sd) * 1.2 + vec3(0.25, 0.3, 0.6) * pow(max(sd, 0.0), 60.0) * 0.2;
        vec3 q = dir * 260.0;
        vec3 cell = floor(q);
        uint h = hashu(uint(int(cell.x)) ^ hashu(uint(int(cell.y)) ^ hashu(uint(int(cell.z)))));
        float r = float(h >> 8) * (1.0 / 16777216.0);
        if (r > 0.996 && e > 0.03) {
            float star = 1.0 - smoothstep(0.0, 0.3, length(fract(q) - 0.5));
            c += vec3(star) * (0.7 + 0.3 * sin(u_tw * 2.0 + r * 4000.0)) * 1.4;
        }
    }
    o = vec4(tone(c), 1.0);
}
