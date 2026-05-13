#include "global_bindings.glsl"
#include "simulation/hybrid_fluid.glsl"
#include "simulation/particles.glsl"

layout(set = 2, binding = 0) buffer restrict ParticlePositionLlBuffer { ParticlePositionLl Particles[]; };
layout(set = 2, binding = 1) buffer restrict readonly ParticleComp { vec4 ParticleBufferVelocityComponent[]; };
layout(set = 2, binding = 2, std430) buffer FluidDualGridCells_ { uint Cells[]; } FluidDualGridCells;
layout(set = 2, binding = 3, r8_snorm) uniform image3D MarkerVolume;
layout(set = 2, binding = 4, r32f) uniform image3D VelocityComponentVolume;

layout(push_constant) uniform PushConstants { uint VelocityTransferComponent; };
