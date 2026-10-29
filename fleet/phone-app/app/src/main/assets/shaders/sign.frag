in vec3 v_world;
in vec3 v_nrm;
in vec2 v_uv;
in float v_mat;
in vec4 v_ls;
uniform sampler2D u_text;
uniform vec3 u_signCol;
out vec4 o;

void main() {
    vec3 base;
    if (v_mat < -0.5) base = pow(texture(u_text, v_uv).rgb, vec3(2.2));
    else if (v_mat < 0.5) base = u_signCol;
    else base = palette(14.0);
    vec3 c = shade(base, normalize(v_nrm), v_world, v_ls, 0.15);
    if (v_mat < -0.5) c += base * 0.25;
    o = vec4(finish(c, v_world), 1.0);
}
