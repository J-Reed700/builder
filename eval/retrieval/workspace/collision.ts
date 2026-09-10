// Resolve overlap between player and boss bodies.
function resolve_boss_collision(player: Body, boss: Body) {
    separate_bodies(player, boss);
}

function render_boss_sprite(boss: Body) { draw(boss); }
