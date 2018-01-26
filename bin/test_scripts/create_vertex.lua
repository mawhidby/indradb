local queries = require("queries");
local id = trans:create_vertex("foo");
local vertices = trans:get_vertices(queries.VertexQuery.vertices({id}));
assert(vertices[1].id == id);
assert(vertices[1].type == "foo");
