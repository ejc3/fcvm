package dev.fcvm.robotpark.scene

/**
 * The GLSL programs, assembled from the files in app/src/main/assets/shaders. The app reads
 * them from its assets; the scene tests read the same files and compile every program with
 * glslangValidator, so a shader that does not compile fails the build, not the phone.
 *
 * Determinism rules for every shader here: no derivatives (dFdx, fwidth) and no mipmapped
 * textures, since a 2x2 pixel quad straddles different whole-image pixels on phones whose
 * viewport offsets differ by an odd number; no time but the uniforms; random numbers only from
 * the integer hash of ids and whole-image positions.
 */
object Shaders {
    const val HEADER = "#version 300 es\nprecision highp float;\nprecision highp int;\n"

    class Program(val name: String, val vertex: List<String>, val fragment: List<String>, val defines: List<String> = emptyList())

    val PROGRAMS = listOf(
        Program("robot", listOf("common.glsl", "pose.glsl", "robot.vert"), listOf("common.glsl", "light.glsl", "lit.frag")),
        Program("robot_shadow", listOf("common.glsl", "pose.glsl", "robot.vert"), listOf("depth.frag"), listOf("SHADOW")),
        Program("prop", listOf("common.glsl", "prop.vert"), listOf("common.glsl", "light.glsl", "lit.frag")),
        Program("prop_shadow", listOf("common.glsl", "prop.vert"), listOf("depth.frag"), listOf("SHADOW")),
        Program("ground", listOf("common.glsl", "ground.vert"), listOf("common.glsl", "light.glsl", "ground.frag")),
        Program("sky", listOf("sky.vert"), listOf("common.glsl", "sky.frag")),
        Program("particle", listOf("common.glsl", "pose.glsl", "particle.vert"), listOf("common.glsl", "particle.frag")),
        Program("sign", listOf("common.glsl", "sign.vert"), listOf("common.glsl", "light.glsl", "sign.frag")),
        Program("sign_shadow", listOf("common.glsl", "sign.vert"), listOf("depth.frag"), listOf("SHADOW")),
    )

    fun source(files: List<String>, defines: List<String>, read: (String) -> String): String =
        HEADER + defines.joinToString("") { "#define $it 1\n" } + files.joinToString("\n") { "// ---- $it\n" + read(it) }
}
