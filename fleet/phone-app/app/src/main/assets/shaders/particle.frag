// Premultiplied alpha: alpha 0 adds light, alpha 1 covers.
in vec2 v_uv;
in vec4 v_col;
in float v_shape;
out vec4 o;

void main() {
    float r = length(v_uv);
    if (v_shape < 0.5) {
        o = vec4(tone(v_col.rgb), 1.0);
    } else if (v_shape < 1.5) {
        if (r > 1.0) discard;
        float ring = smoothstep(0.72, 0.93, r) * (1.0 - smoothstep(0.93, 1.0, r));
        float hi = 1.0 - smoothstep(0.0, 0.22, length(v_uv - vec2(-0.38, 0.38)));
        float a = clamp(ring * 0.8 + hi * 0.9 + 0.06, 0.0, 1.0) * v_col.a;
        o = vec4(tone(v_col.rgb * 1.4) * a, a);
    } else if (v_shape < 2.5) {
        float g = exp(-r * r * 5.0) * (1.0 - smoothstep(0.8, 1.0, r));
        o = vec4(tone(v_col.rgb) * g, 0.0);
    } else {
        float d = min(abs(v_uv.x), abs(v_uv.y));
        float g = (1.0 - smoothstep(0.0, 0.14, d)) * (1.0 - r) + exp(-r * r * 12.0);
        o = vec4(tone(v_col.rgb) * clamp(g, 0.0, 1.0), 0.0);
    }
}
