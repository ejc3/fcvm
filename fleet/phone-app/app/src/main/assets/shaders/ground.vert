layout(location = 0) in vec3 a_pos;
out vec3 v_world;
out vec4 v_ls;

void main() {
    v_world = a_pos;
    v_ls = u_lightViewProj * vec4(a_pos, 1.0);
    gl_Position = u_viewProj * vec4(a_pos, 1.0);
}
