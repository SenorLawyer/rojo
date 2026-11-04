local Rojo = script:FindFirstAncestor("Rojo")
local Plugin = Rojo.Plugin
local Packages = Rojo.Packages

local Roact = require(Packages.Roact)

local Theme = require(Plugin.App.Theme)

local Spinner = require(Plugin.App.Components.Spinner)
local TextButton = require(Plugin.App.Components.TextButton)

local e = Roact.createElement

local ConnectingPage = Roact.Component:extend("ConnectingPage")

function ConnectingPage:render()
	return Theme.with(function(theme)
		return e("Frame", {
			Size = UDim2.new(1, 0, 1, 0),
			BackgroundTransparency = 1,
		}, {
			Spinner = e(Spinner, {
				position = UDim2.new(0.5, 0, 0.5, 0),
				anchorPoint = Vector2.new(0.5, 0.5),
				transparency = self.props.transparency,
			}),
			Text = if type(self.props.text) == "string" and #self.props.text > 0
				then e("TextLabel", {
					Text = self.props.text,
					Position = UDim2.new(0.5, 0, 0.5, 30),
					Size = UDim2.new(1, -40, 0.5, -100),
					AnchorPoint = Vector2.new(0.5, 0),
					TextXAlignment = Enum.TextXAlignment.Center,
					TextYAlignment = Enum.TextYAlignment.Top,
					RichText = true,
					FontFace = theme.Font.Thin,
					TextSize = theme.TextSize.Medium,
					TextColor3 = theme.SubTextColor,
					TextTruncate = Enum.TextTruncate.AtEnd,
					TextTransparency = self.props.transparency,
					BackgroundTransparency = 1,
				}, {
					-- The scale-based height reaches zero in a short widget,
					-- which would hide the waiting text entirely.
					SizeConstraint = e("UISizeConstraint", {
						MinSize = Vector2.new(0, theme.TextSize.Medium * 3),
					}),
				})
				else nil,
			Cancel = e(TextButton, {
				text = "Cancel",
				style = "Bordered",
				position = UDim2.new(0.5, 0, 1, -20),
				anchorPoint = Vector2.new(0.5, 1),
				transparency = self.props.transparency,
				onClick = self.props.onCancel,
			}),
		})
	end)
end

return ConnectingPage
