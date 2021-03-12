#version 400 core

// Index in the textures[] uniform.
flat in int texId;

// Texture coordinates.
in vec2 texCoords;

// Array with graphics data.
uniform sampler2D textures[32];

// Computed color.
out vec4 color;

void main() {
    if(texId < 32) {
        color = texture(textures[texId], texCoords);
    } else {
        discard;
    }
}
